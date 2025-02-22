use assert2::assert as fancy_assert;
use coe::Coerce;
use core::slice;
use dyn_stack::{DynStack, SizeOverflow, StackReq};
use faer_core::{
    c32, c64, householder::make_householder_in_place, mul::matmul, temp_mat_req, temp_mat_uninit,
    temp_mat_zeroed, zip, ColMut, ColRef, ComplexField, Conj, MatMut, MatRef, Parallelism, RowMut,
    RowRef,
};
use pulp::Simd;
use reborrow::*;

pub fn bidiagonalize_in_place_req<T: 'static>(
    m: usize,
    n: usize,
    parallelism: Parallelism,
) -> Result<StackReq, SizeOverflow> {
    StackReq::try_all_of([
        temp_mat_req::<T>(n, 1)?,
        temp_mat_req::<T>(m, 1)?,
        temp_mat_req::<T>(
            m,
            match parallelism {
                Parallelism::None => 1,
                Parallelism::Rayon(n_threads) => {
                    if n_threads == 0 {
                        rayon::current_num_threads()
                    } else {
                        n_threads
                    }
                }
            },
        )?,
    ])
}

pub fn bidiagonalize_in_place<T: ComplexField>(
    mut a: MatMut<'_, T>,
    mut householder_left: ColMut<'_, T>,
    mut householder_right: ColMut<'_, T>,
    parallelism: Parallelism,
    mut stack: DynStack<'_>,
) {
    let m = a.nrows();
    let n = a.ncols();

    fancy_assert!(m >= n);

    let n_threads = match parallelism {
        Parallelism::None => 1,
        Parallelism::Rayon(n_threads) => {
            if n_threads == 0 {
                rayon::current_num_threads()
            } else {
                n_threads
            }
        }
    };

    let (mut y, mut stack) = unsafe { temp_mat_uninit::<T>(n, 1, stack.rb_mut()) };
    let mut y = y.as_mut();
    let (mut z, mut stack) = unsafe { temp_mat_uninit::<T>(m, 1, stack.rb_mut()) };
    let mut z = z.as_mut();

    let (mut z_tmp, _) = temp_mat_zeroed::<T>(m, n_threads, stack.rb_mut());
    let mut z_tmp = z_tmp.as_mut();

    let mut tl = T::zero();
    let mut tr = T::zero();
    let mut a01 = T::zero();

    let arch = pulp::Arch::new();
    for k in 0..n {
        let (a_left, a_right) = a.rb_mut().split_at_col(k);
        let (mut a_top, mut a_cur) = a_right.split_at_row(k);

        let m = a_cur.nrows();
        let n = a_cur.ncols();

        let (mut a_col, a_right) = a_cur.rb_mut().split_at_col(1);
        let (mut a_row, mut a_next) = a_right.split_at_row(1);

        if k > 0 {
            let u = a_left.rb().submatrix(k, k - 1, m, 1);
            let mut v = a_top.rb_mut().submatrix(k - 1, 0, 1, n);
            let y = y.rb().submatrix(k - 1, 0, n, 1);
            let z = z.rb().submatrix(k - 1, 0, m, 1);

            let f0 = y[(0, 0)].conj().mul(&tl.inv());
            let f1 = v[(0, 0)].conj().mul(&tr.inv());

            zip!(a_col.rb_mut(), u, z).for_each(|a, b, c| *a = a.sub(&f0.mul(b)).sub(&f1.mul(c)));

            let f0 = u[(0, 0)].mul(&tl.inv());
            let f1 = z[(0, 0)].mul(&tr.inv());
            zip!(
                a_row.rb_mut(),
                y.submatrix(1, 0, n - 1, 1).transpose(),
                v.rb().submatrix(0, 1, 1, n - 1),
            )
            .for_each(|a, b, c| *a = a.sub(&f0.mul(&(*b).conj())).sub(&f1.mul(&(*c).conj())));

            v[(0, 0)] = a01;
        }

        let mut y = y.rb_mut().submatrix(k, 0, n - 1, 1);
        let mut z = z.rb_mut().submatrix(k, 0, m - 1, 1);
        let z_tmp = z_tmp.rb_mut().submatrix(k, 0, m - 1, n_threads);

        let tl_prev = tl.clone();
        let a00;
        (tl, a00) = {
            let head = a_col[(0, 0)].clone();
            let essential = a_col.rb_mut().col(0).subrows(1, m - 1);
            let mut tail_squared_norm = T::Real::zero();
            for x in essential.rb() {
                tail_squared_norm = tail_squared_norm.add(&x.mul(&x.conj()).real());
            }
            make_householder_in_place(Some(essential), head, tail_squared_norm)
        };
        a_col[(0, 0)] = a00;
        householder_left.rb_mut().write_at(k, tl.clone());

        if n == 1 {
            break;
        }

        let u = a_col.rb().submatrix(1, 0, m - 1, 1);

        bidiag_fused_op(
            k,
            m,
            n,
            tl_prev,
            tl.clone(),
            tr.clone(),
            z_tmp,
            a_left,
            a_top,
            a_next.rb_mut(),
            y.rb_mut(),
            parallelism,
            z.rb_mut(),
            u,
            a_row.rb_mut(),
        );

        (tr, a01) = {
            let head = a_row[(0, 0)].clone();
            let essential = a_row.rb().row(0).subcols(1, n - 2).transpose();
            let mut tail_squared_norm = T::Real::zero();
            for x in essential {
                tail_squared_norm = tail_squared_norm.add(&x.mul(&x.conj()).real());
            }
            make_householder_in_place(None, head, tail_squared_norm)
        };
        householder_right.rb_mut().write_at(k, tr.clone());

        let diff = a_row[(0, 0)].sub(&a01);

        if diff != T::zero() {
            let f = diff.inv().conj();
            a_row
                .rb_mut()
                .row(0)
                .subcols(1, n - 2)
                .transpose()
                .cwise()
                .for_each(|x| *x = (*x).conj().mul(&f));

            zip!(z.rb_mut().col(0), a_next.rb().col(0))
                .for_each(|z, a| *z = f.mul(&z.sub(&a01.conj().mul(a))));
        }

        a_row[(0, 0)] = T::one();
        let b = faer_core::mul::dot(arch, y.rb().col(0), a_row.rb().row(0).transpose());

        let factor = b.mul(&tl.inv()).neg();
        zip!(z.rb_mut(), u).for_each(|z, u| *z = z.add(&(*u).mul(&factor)));
    }
}

#[allow(dead_code)]
fn bidiag_fused_op_reference<T: ComplexField>(
    k: usize,
    m: usize,
    n: usize,
    tl_prev: T,
    tl: T,
    tr: T,
    _z_tmp: MatMut<'_, T>,
    a_left: MatMut<'_, T>,
    a_top: MatMut<'_, T>,
    mut a_next: MatMut<'_, T>,
    mut y: MatMut<'_, T>,
    parallelism: Parallelism,
    mut z: MatMut<'_, T>,
    u: MatRef<'_, T>,
    mut a_row: MatMut<'_, T>,
) {
    if k > 0 {
        let u_prev = a_left.rb().submatrix(k + 1, k - 1, m - 1, 1);
        let v_prev = a_top.rb().submatrix(k - 1, 1, 1, n - 1);
        matmul(
            a_next.rb_mut(),
            Conj::No,
            u_prev,
            Conj::No,
            y.rb().transpose(),
            Conj::Yes,
            Some(T::one()),
            tl_prev.inv().neg(),
            parallelism,
        );
        matmul(
            a_next.rb_mut(),
            Conj::No,
            z.rb(),
            Conj::No,
            v_prev,
            Conj::Yes,
            Some(T::one()),
            tr.inv().neg(),
            parallelism,
        );

        matmul(
            y.rb_mut(),
            Conj::No,
            a_next.rb().transpose(),
            Conj::Yes,
            u,
            Conj::No,
            None,
            T::one(),
            parallelism,
        );
        zip!(y.rb_mut(), a_row.rb().transpose())
            .for_each(|dst, src| *dst = (*dst).add(&src.conj()));
        let tl_inv = tl.inv();
        zip!(a_row.rb_mut(), y.rb().transpose())
            .for_each(|dst, src| *dst = dst.sub(&src.conj().mul(&tl_inv)));
        matmul(
            z.rb_mut(),
            Conj::No,
            a_next.rb(),
            Conj::No,
            a_row.rb().transpose(),
            Conj::Yes,
            None,
            T::one(),
            parallelism,
        );
    } else {
        matmul(
            y.rb_mut(),
            Conj::No,
            a_next.rb().transpose(),
            Conj::Yes,
            u,
            Conj::No,
            None,
            T::one(),
            parallelism,
        );
        zip!(y.rb_mut(), a_row.rb().transpose()).for_each(|dst, src| *dst = dst.add(&src.conj()));
        let tl_inv = tl.inv();
        zip!(a_row.rb_mut(), y.rb().transpose())
            .for_each(|dst, src| *dst = dst.sub(&src.conj().mul(&tl_inv)));
        matmul(
            z.rb_mut(),
            Conj::No,
            a_next.rb(),
            Conj::No,
            a_row.rb().transpose(),
            Conj::Yes,
            None,
            T::one(),
            parallelism,
        );
    }
}

fn bidiag_fused_op_step0_f64(
    arch: pulp::Arch,

    // update a_next
    a_j: &mut [f64],
    z: &[f64],
    u_prev: &[f64],
    u_rhs: f64,
    z_rhs: f64,

    // compute yj
    u: &[f64],
) -> f64 {
    struct Impl<'a> {
        a_j: &'a mut [f64],
        z: &'a [f64],
        u_prev: &'a [f64],
        u_rhs: f64,
        z_rhs: f64,
        u: &'a [f64],
    }

    impl pulp::WithSimd for Impl<'_> {
        type Output = f64;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self {
                a_j,
                z,
                u_prev,
                u_rhs,
                z_rhs,
                u,
            } = self;
            let (a_j_head, a_j_tail) = S::f64s_as_mut_simd(a_j);
            let (z_head, z_tail) = S::f64s_as_simd(z);
            let (u_prev_head, u_prev_tail) = S::f64s_as_simd(u_prev);
            let (u_head, u_tail) = S::f64s_as_simd(u);

            let (a_j_head4, a_j_head1) = pulp::as_arrays_mut::<4, _>(a_j_head);
            let (z_head4, z_head1) = pulp::as_arrays::<4, _>(z_head);
            let (u_prev_head4, u_prev_head1) = pulp::as_arrays::<4, _>(u_prev_head);
            let (u_head4, u_head1) = pulp::as_arrays::<4, _>(u_head);

            let mut sum_v0 = simd.f64s_splat(0.0_f64);
            let mut sum_v1 = simd.f64s_splat(0.0_f64);
            let mut sum_v2 = simd.f64s_splat(0.0_f64);
            let mut sum_v3 = simd.f64s_splat(0.0_f64);

            let u_rhs_v = simd.f64s_splat(u_rhs);
            let z_rhs_v = simd.f64s_splat(z_rhs);

            for (((aij, zi), u_prev_i), ui) in a_j_head4
                .iter_mut()
                .zip(z_head4)
                .zip(u_prev_head4)
                .zip(u_head4)
            {
                let aij_new0 = simd.f64s_mul_adde(
                    u_prev_i[0],
                    simd.f64s_neg(u_rhs_v),
                    simd.f64s_mul_adde(zi[0], simd.f64s_neg(z_rhs_v), aij[0]),
                );
                let aij_new1 = simd.f64s_mul_adde(
                    u_prev_i[1],
                    simd.f64s_neg(u_rhs_v),
                    simd.f64s_mul_adde(zi[1], simd.f64s_neg(z_rhs_v), aij[1]),
                );
                let aij_new2 = simd.f64s_mul_adde(
                    u_prev_i[2],
                    simd.f64s_neg(u_rhs_v),
                    simd.f64s_mul_adde(zi[2], simd.f64s_neg(z_rhs_v), aij[2]),
                );
                let aij_new3 = simd.f64s_mul_adde(
                    u_prev_i[3],
                    simd.f64s_neg(u_rhs_v),
                    simd.f64s_mul_adde(zi[3], simd.f64s_neg(z_rhs_v), aij[3]),
                );
                sum_v0 = simd.f64s_mul_adde(aij_new0, ui[0], sum_v0);
                sum_v1 = simd.f64s_mul_adde(aij_new1, ui[1], sum_v1);
                sum_v2 = simd.f64s_mul_adde(aij_new2, ui[2], sum_v2);
                sum_v3 = simd.f64s_mul_adde(aij_new3, ui[3], sum_v3);
                aij[0] = aij_new0;
                aij[1] = aij_new1;
                aij[2] = aij_new2;
                aij[3] = aij_new3;
            }

            sum_v0 = simd.f64s_add(sum_v0, sum_v1);
            sum_v2 = simd.f64s_add(sum_v2, sum_v3);
            sum_v0 = simd.f64s_add(sum_v0, sum_v2);

            for (((aij, zi), u_prev_i), ui) in a_j_head1
                .iter_mut()
                .zip(z_head1)
                .zip(u_prev_head1)
                .zip(u_head1)
            {
                let aij_new = simd.f64s_mul_adde(
                    *u_prev_i,
                    simd.f64s_neg(u_rhs_v),
                    simd.f64s_mul_adde(*zi, simd.f64s_neg(z_rhs_v), *aij),
                );
                sum_v0 = simd.f64s_mul_adde(aij_new, *ui, sum_v0);
                *aij = aij_new;
            }

            let mut sum = simd.f64s_reduce_sum(sum_v0);

            for (((aij, zi), u_prev_i), ui) in
                a_j_tail.iter_mut().zip(z_tail).zip(u_prev_tail).zip(u_tail)
            {
                let aij_new = f64::mul_add(*u_prev_i, -u_rhs, f64::mul_add(*zi, -z_rhs, *aij));
                sum = f64::mul_add(aij_new, *ui, sum);
                *aij = aij_new;
            }

            sum
        }
    }

    arch.dispatch(Impl {
        a_j,
        z,
        u_prev,
        u_rhs,
        z_rhs,
        u,
    })
}

fn bidiag_fused_op_step1_f64(
    arch: pulp::Arch,

    // update z
    z: &mut [f64],
    a_j: &[f64],
    rhs: f64,
) {
    struct Impl<'a> {
        z: &'a mut [f64],
        a_j: &'a [f64],
        rhs: f64,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = ();

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { z, a_j, rhs } = self;
            let (z_head, z_tail) = S::f64s_as_mut_simd(z);
            let (a_j_head, a_j_tail) = S::f64s_as_simd(a_j);
            let rhs_v = simd.f64s_splat(rhs);

            for (zi, aij) in z_head.iter_mut().zip(a_j_head) {
                *zi = simd.f64s_mul_adde(*aij, rhs_v, *zi);
            }
            for (zi, aij) in z_tail.iter_mut().zip(a_j_tail) {
                *zi = f64::mul_add(*aij, rhs, *zi);
            }
        }
    }
    arch.dispatch(Impl { z, a_j, rhs })
}

fn bidiag_fused_op_step0_c64(
    arch: pulp::Arch,

    // update a_next
    a_j: &mut [c64],
    z: &[c64],
    u_prev: &[c64],
    u_rhs: c64,
    z_rhs: c64,

    // compute yj
    u: &[c64],
) -> c64 {
    struct Impl<'a> {
        a_j: &'a mut [c64],
        z: &'a [c64],
        u_prev: &'a [c64],
        u_rhs: c64,
        z_rhs: c64,
        u: &'a [c64],
    }

    impl pulp::WithSimd for Impl<'_> {
        type Output = c64;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self {
                a_j,
                z,
                u_prev,
                u_rhs,
                z_rhs,
                u,
            } = self;
            let (a_j_head, a_j_tail) = S::c64s_as_mut_simd(a_j);
            let (z_head, z_tail) = S::c64s_as_simd(z);
            let (u_prev_head, u_prev_tail) = S::c64s_as_simd(u_prev);
            let (u_head, u_tail) = S::c64s_as_simd(u);

            let (a_j_head4, a_j_head1) = pulp::as_arrays_mut::<4, _>(a_j_head);
            let (z_head4, z_head1) = pulp::as_arrays::<4, _>(z_head);
            let (u_prev_head4, u_prev_head1) = pulp::as_arrays::<4, _>(u_prev_head);
            let (u_head4, u_head1) = pulp::as_arrays::<4, _>(u_head);

            let mut sum_v0 = simd.c64s_splat(c64::zero());
            let mut sum_v1 = simd.c64s_splat(c64::zero());
            let mut sum_v2 = simd.c64s_splat(c64::zero());
            let mut sum_v3 = simd.c64s_splat(c64::zero());

            let u_rhs_v = simd.c64s_neg(simd.c64s_splat(u_rhs));
            let z_rhs_v = simd.c64s_neg(simd.c64s_splat(z_rhs));

            for (((aij, zi), u_prev_i), ui) in a_j_head4
                .iter_mut()
                .zip(z_head4)
                .zip(u_prev_head4)
                .zip(u_head4)
            {
                let aij_new0 = simd.c64s_mul_adde(
                    u_prev_i[0],
                    u_rhs_v,
                    simd.c64s_mul_adde(zi[0], z_rhs_v, aij[0]),
                );
                let aij_new1 = simd.c64s_mul_adde(
                    u_prev_i[1],
                    u_rhs_v,
                    simd.c64s_mul_adde(zi[1], z_rhs_v, aij[1]),
                );
                let aij_new2 = simd.c64s_mul_adde(
                    u_prev_i[2],
                    u_rhs_v,
                    simd.c64s_mul_adde(zi[2], z_rhs_v, aij[2]),
                );
                let aij_new3 = simd.c64s_mul_adde(
                    u_prev_i[3],
                    u_rhs_v,
                    simd.c64s_mul_adde(zi[3], z_rhs_v, aij[3]),
                );
                sum_v0 = simd.c64s_conj_mul_adde(aij_new0, ui[0], sum_v0);
                sum_v1 = simd.c64s_conj_mul_adde(aij_new1, ui[1], sum_v1);
                sum_v2 = simd.c64s_conj_mul_adde(aij_new2, ui[2], sum_v2);
                sum_v3 = simd.c64s_conj_mul_adde(aij_new3, ui[3], sum_v3);
                aij[0] = aij_new0;
                aij[1] = aij_new1;
                aij[2] = aij_new2;
                aij[3] = aij_new3;
            }

            sum_v0 = simd.c64s_add(sum_v0, sum_v1);
            sum_v2 = simd.c64s_add(sum_v2, sum_v3);
            sum_v0 = simd.c64s_add(sum_v0, sum_v2);

            for (((aij, zi), u_prev_i), ui) in a_j_head1
                .iter_mut()
                .zip(z_head1)
                .zip(u_prev_head1)
                .zip(u_head1)
            {
                let aij_new =
                    simd.c64s_mul_adde(*u_prev_i, u_rhs_v, simd.c64s_mul_adde(*zi, z_rhs_v, *aij));
                sum_v0 = simd.c64s_conj_mul_adde(aij_new, *ui, sum_v0);
                *aij = aij_new;
            }

            let mut sum = simd.c64s_reduce_sum(sum_v0);

            for (((aij, zi), u_prev_i), ui) in
                a_j_tail.iter_mut().zip(z_tail).zip(u_prev_tail).zip(u_tail)
            {
                let aij_new = *aij - *u_prev_i * u_rhs - *zi * z_rhs;
                sum = aij_new.conj() * *ui + sum;
                *aij = aij_new;
            }

            sum
        }
    }

    arch.dispatch(Impl {
        a_j,
        z,
        u_prev,
        u_rhs,
        z_rhs,
        u,
    })
}

fn bidiag_fused_op_step1_c64(
    arch: pulp::Arch,

    // update z
    z: &mut [c64],
    a_j: &[c64],
    rhs: c64,
) {
    struct Impl<'a> {
        z: &'a mut [c64],
        a_j: &'a [c64],
        rhs: c64,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = ();

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { z, a_j, rhs } = self;
            let (z_head, z_tail) = S::c64s_as_mut_simd(z);
            let (a_j_head, a_j_tail) = S::c64s_as_simd(a_j);
            let rhs_v = simd.c64s_splat(rhs);

            for (zi, aij) in z_head.iter_mut().zip(a_j_head) {
                *zi = simd.c64s_mul_adde(*aij, rhs_v, *zi);
            }
            for (zi, aij) in z_tail.iter_mut().zip(a_j_tail) {
                *zi = *aij * rhs + *zi;
            }
        }
    }
    arch.dispatch(Impl { z, a_j, rhs })
}

fn bidiag_fused_op_step0_f32(
    arch: pulp::Arch,

    // update a_next
    a_j: &mut [f32],
    z: &[f32],
    u_prev: &[f32],
    u_rhs: f32,
    z_rhs: f32,

    // compute yj
    u: &[f32],
) -> f32 {
    struct Impl<'a> {
        a_j: &'a mut [f32],
        z: &'a [f32],
        u_prev: &'a [f32],
        u_rhs: f32,
        z_rhs: f32,
        u: &'a [f32],
    }

    impl pulp::WithSimd for Impl<'_> {
        type Output = f32;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self {
                a_j,
                z,
                u_prev,
                u_rhs,
                z_rhs,
                u,
            } = self;
            let (a_j_head, a_j_tail) = S::f32s_as_mut_simd(a_j);
            let (z_head, z_tail) = S::f32s_as_simd(z);
            let (u_prev_head, u_prev_tail) = S::f32s_as_simd(u_prev);
            let (u_head, u_tail) = S::f32s_as_simd(u);

            let (a_j_head4, a_j_head1) = pulp::as_arrays_mut::<4, _>(a_j_head);
            let (z_head4, z_head1) = pulp::as_arrays::<4, _>(z_head);
            let (u_prev_head4, u_prev_head1) = pulp::as_arrays::<4, _>(u_prev_head);
            let (u_head4, u_head1) = pulp::as_arrays::<4, _>(u_head);

            let mut sum_v0 = simd.f32s_splat(0.0_f32);
            let mut sum_v1 = simd.f32s_splat(0.0_f32);
            let mut sum_v2 = simd.f32s_splat(0.0_f32);
            let mut sum_v3 = simd.f32s_splat(0.0_f32);

            let u_rhs_v = simd.f32s_splat(u_rhs);
            let z_rhs_v = simd.f32s_splat(z_rhs);

            for (((aij, zi), u_prev_i), ui) in a_j_head4
                .iter_mut()
                .zip(z_head4)
                .zip(u_prev_head4)
                .zip(u_head4)
            {
                let aij_new0 = simd.f32s_mul_adde(
                    u_prev_i[0],
                    simd.f32s_neg(u_rhs_v),
                    simd.f32s_mul_adde(zi[0], simd.f32s_neg(z_rhs_v), aij[0]),
                );
                let aij_new1 = simd.f32s_mul_adde(
                    u_prev_i[1],
                    simd.f32s_neg(u_rhs_v),
                    simd.f32s_mul_adde(zi[1], simd.f32s_neg(z_rhs_v), aij[1]),
                );
                let aij_new2 = simd.f32s_mul_adde(
                    u_prev_i[2],
                    simd.f32s_neg(u_rhs_v),
                    simd.f32s_mul_adde(zi[2], simd.f32s_neg(z_rhs_v), aij[2]),
                );
                let aij_new3 = simd.f32s_mul_adde(
                    u_prev_i[3],
                    simd.f32s_neg(u_rhs_v),
                    simd.f32s_mul_adde(zi[3], simd.f32s_neg(z_rhs_v), aij[3]),
                );
                sum_v0 = simd.f32s_mul_adde(aij_new0, ui[0], sum_v0);
                sum_v1 = simd.f32s_mul_adde(aij_new1, ui[1], sum_v1);
                sum_v2 = simd.f32s_mul_adde(aij_new2, ui[2], sum_v2);
                sum_v3 = simd.f32s_mul_adde(aij_new3, ui[3], sum_v3);
                aij[0] = aij_new0;
                aij[1] = aij_new1;
                aij[2] = aij_new2;
                aij[3] = aij_new3;
            }

            sum_v0 = simd.f32s_add(sum_v0, sum_v1);
            sum_v2 = simd.f32s_add(sum_v2, sum_v3);
            sum_v0 = simd.f32s_add(sum_v0, sum_v2);

            for (((aij, zi), u_prev_i), ui) in a_j_head1
                .iter_mut()
                .zip(z_head1)
                .zip(u_prev_head1)
                .zip(u_head1)
            {
                let aij_new = simd.f32s_mul_adde(
                    *u_prev_i,
                    simd.f32s_neg(u_rhs_v),
                    simd.f32s_mul_adde(*zi, simd.f32s_neg(z_rhs_v), *aij),
                );
                sum_v0 = simd.f32s_mul_adde(aij_new, *ui, sum_v0);
                *aij = aij_new;
            }

            let mut sum = simd.f32s_reduce_sum(sum_v0);

            for (((aij, zi), u_prev_i), ui) in
                a_j_tail.iter_mut().zip(z_tail).zip(u_prev_tail).zip(u_tail)
            {
                let aij_new = f32::mul_add(*u_prev_i, -u_rhs, f32::mul_add(*zi, -z_rhs, *aij));
                sum = f32::mul_add(aij_new, *ui, sum);
                *aij = aij_new;
            }

            sum
        }
    }

    arch.dispatch(Impl {
        a_j,
        z,
        u_prev,
        u_rhs,
        z_rhs,
        u,
    })
}

fn bidiag_fused_op_step1_f32(
    arch: pulp::Arch,

    // update z
    z: &mut [f32],
    a_j: &[f32],
    rhs: f32,
) {
    struct Impl<'a> {
        z: &'a mut [f32],
        a_j: &'a [f32],
        rhs: f32,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = ();

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { z, a_j, rhs } = self;
            let (z_head, z_tail) = S::f32s_as_mut_simd(z);
            let (a_j_head, a_j_tail) = S::f32s_as_simd(a_j);
            let rhs_v = simd.f32s_splat(rhs);

            for (zi, aij) in z_head.iter_mut().zip(a_j_head) {
                *zi = simd.f32s_mul_adde(*aij, rhs_v, *zi);
            }
            for (zi, aij) in z_tail.iter_mut().zip(a_j_tail) {
                *zi = f32::mul_add(*aij, rhs, *zi);
            }
        }
    }
    arch.dispatch(Impl { z, a_j, rhs })
}

fn bidiag_fused_op_step0_c32(
    arch: pulp::Arch,

    // update a_next
    a_j: &mut [c32],
    z: &[c32],
    u_prev: &[c32],
    u_rhs: c32,
    z_rhs: c32,

    // compute yj
    u: &[c32],
) -> c32 {
    struct Impl<'a> {
        a_j: &'a mut [c32],
        z: &'a [c32],
        u_prev: &'a [c32],
        u_rhs: c32,
        z_rhs: c32,
        u: &'a [c32],
    }

    impl pulp::WithSimd for Impl<'_> {
        type Output = c32;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self {
                a_j,
                z,
                u_prev,
                u_rhs,
                z_rhs,
                u,
            } = self;
            let (a_j_head, a_j_tail) = S::c32s_as_mut_simd(a_j);
            let (z_head, z_tail) = S::c32s_as_simd(z);
            let (u_prev_head, u_prev_tail) = S::c32s_as_simd(u_prev);
            let (u_head, u_tail) = S::c32s_as_simd(u);

            let (a_j_head4, a_j_head1) = pulp::as_arrays_mut::<4, _>(a_j_head);
            let (z_head4, z_head1) = pulp::as_arrays::<4, _>(z_head);
            let (u_prev_head4, u_prev_head1) = pulp::as_arrays::<4, _>(u_prev_head);
            let (u_head4, u_head1) = pulp::as_arrays::<4, _>(u_head);

            let mut sum_v0 = simd.c32s_splat(c32::zero());
            let mut sum_v1 = simd.c32s_splat(c32::zero());
            let mut sum_v2 = simd.c32s_splat(c32::zero());
            let mut sum_v3 = simd.c32s_splat(c32::zero());

            let u_rhs_v = simd.c32s_neg(simd.c32s_splat(u_rhs));
            let z_rhs_v = simd.c32s_neg(simd.c32s_splat(z_rhs));

            for (((aij, zi), u_prev_i), ui) in a_j_head4
                .iter_mut()
                .zip(z_head4)
                .zip(u_prev_head4)
                .zip(u_head4)
            {
                let aij_new0 = simd.c32s_mul_adde(
                    u_prev_i[0],
                    u_rhs_v,
                    simd.c32s_mul_adde(zi[0], z_rhs_v, aij[0]),
                );
                let aij_new1 = simd.c32s_mul_adde(
                    u_prev_i[1],
                    u_rhs_v,
                    simd.c32s_mul_adde(zi[1], z_rhs_v, aij[1]),
                );
                let aij_new2 = simd.c32s_mul_adde(
                    u_prev_i[2],
                    u_rhs_v,
                    simd.c32s_mul_adde(zi[2], z_rhs_v, aij[2]),
                );
                let aij_new3 = simd.c32s_mul_adde(
                    u_prev_i[3],
                    u_rhs_v,
                    simd.c32s_mul_adde(zi[3], z_rhs_v, aij[3]),
                );
                sum_v0 = simd.c32s_conj_mul_adde(aij_new0, ui[0], sum_v0);
                sum_v1 = simd.c32s_conj_mul_adde(aij_new1, ui[1], sum_v1);
                sum_v2 = simd.c32s_conj_mul_adde(aij_new2, ui[2], sum_v2);
                sum_v3 = simd.c32s_conj_mul_adde(aij_new3, ui[3], sum_v3);
                aij[0] = aij_new0;
                aij[1] = aij_new1;
                aij[2] = aij_new2;
                aij[3] = aij_new3;
            }

            sum_v0 = simd.c32s_add(sum_v0, sum_v1);
            sum_v2 = simd.c32s_add(sum_v2, sum_v3);
            sum_v0 = simd.c32s_add(sum_v0, sum_v2);

            for (((aij, zi), u_prev_i), ui) in a_j_head1
                .iter_mut()
                .zip(z_head1)
                .zip(u_prev_head1)
                .zip(u_head1)
            {
                let aij_new =
                    simd.c32s_mul_adde(*u_prev_i, u_rhs_v, simd.c32s_mul_adde(*zi, z_rhs_v, *aij));
                sum_v0 = simd.c32s_conj_mul_adde(aij_new, *ui, sum_v0);
                *aij = aij_new;
            }

            let mut sum = simd.c32s_reduce_sum(sum_v0);

            for (((aij, zi), u_prev_i), ui) in
                a_j_tail.iter_mut().zip(z_tail).zip(u_prev_tail).zip(u_tail)
            {
                let aij_new = *aij - *u_prev_i * u_rhs - *zi * z_rhs;
                sum = aij_new.conj() * *ui + sum;
                *aij = aij_new;
            }

            sum
        }
    }

    arch.dispatch(Impl {
        a_j,
        z,
        u_prev,
        u_rhs,
        z_rhs,
        u,
    })
}

fn bidiag_fused_op_step1_c32(
    arch: pulp::Arch,

    // update z
    z: &mut [c32],
    a_j: &[c32],
    rhs: c32,
) {
    struct Impl<'a> {
        z: &'a mut [c32],
        a_j: &'a [c32],
        rhs: c32,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = ();

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { z, a_j, rhs } = self;
            let (z_head, z_tail) = S::c32s_as_mut_simd(z);
            let (a_j_head, a_j_tail) = S::c32s_as_simd(a_j);
            let rhs_v = simd.c32s_splat(rhs);

            for (zi, aij) in z_head.iter_mut().zip(a_j_head) {
                *zi = simd.c32s_mul_adde(*aij, rhs_v, *zi);
            }
            for (zi, aij) in z_tail.iter_mut().zip(a_j_tail) {
                *zi = *aij * rhs + *zi;
            }
        }
    }
    arch.dispatch(Impl { z, a_j, rhs })
}

fn bidiag_fused_op_process_batch<T: ComplexField>(
    arch: pulp::Arch,
    mut z_tmp: ColMut<'_, T>,
    mut a_next: MatMut<'_, T>,
    mut a_row: RowMut<'_, T>,
    u: ColRef<'_, T>,
    u_prev: ColRef<'_, T>,
    v_prev: RowRef<'_, T>,
    mut y: ColMut<'_, T>,
    z: ColRef<'_, T>,
    tl_prev_inv: T,
    tr_prev_inv: T,
    tl_inv: T,
) {
    let ncols = a_next.ncols();
    let nrows = a_next.nrows();
    for j in 0..ncols {
        let u_rhs = y[j].conj().mul(&tl_prev_inv);
        let z_rhs = v_prev[j].conj().mul(&tr_prev_inv);

        let yj = if coe::is_same::<f64, T>() {
            unsafe {
                coe::coerce_static(bidiag_fused_op_step0_f64(
                    arch,
                    slice::from_raw_parts_mut(a_next.rb_mut().ptr_at(0, j), nrows).coerce(),
                    slice::from_raw_parts(z.as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(u_prev.as_ptr(), nrows).coerce(),
                    coe::coerce_static(u_rhs),
                    coe::coerce_static(z_rhs),
                    slice::from_raw_parts(u.as_ptr(), nrows).coerce(),
                ))
            }
        } else if coe::is_same::<c64, T>() {
            unsafe {
                coe::coerce_static(bidiag_fused_op_step0_c64(
                    arch,
                    slice::from_raw_parts_mut(a_next.rb_mut().ptr_at(0, j), nrows).coerce(),
                    slice::from_raw_parts(z.as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(u_prev.as_ptr(), nrows).coerce(),
                    coe::coerce_static(u_rhs),
                    coe::coerce_static(z_rhs),
                    slice::from_raw_parts(u.as_ptr(), nrows).coerce(),
                ))
            }
        } else if coe::is_same::<f32, T>() {
            unsafe {
                coe::coerce_static(bidiag_fused_op_step0_f32(
                    arch,
                    slice::from_raw_parts_mut(a_next.rb_mut().ptr_at(0, j), nrows).coerce(),
                    slice::from_raw_parts(z.as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(u_prev.as_ptr(), nrows).coerce(),
                    coe::coerce_static(u_rhs),
                    coe::coerce_static(z_rhs),
                    slice::from_raw_parts(u.as_ptr(), nrows).coerce(),
                ))
            }
        } else if coe::is_same::<c32, T>() {
            unsafe {
                coe::coerce_static(bidiag_fused_op_step0_c32(
                    arch,
                    slice::from_raw_parts_mut(a_next.rb_mut().ptr_at(0, j), nrows).coerce(),
                    slice::from_raw_parts(z.as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(u_prev.as_ptr(), nrows).coerce(),
                    coe::coerce_static(u_rhs),
                    coe::coerce_static(z_rhs),
                    slice::from_raw_parts(u.as_ptr(), nrows).coerce(),
                ))
            }
        } else {
            let mut yj = T::zero();
            for i in 0..nrows {
                unsafe {
                    let aij = a_next.rb_mut().get_unchecked(i, j);
                    *aij = aij
                        .sub(&u_prev.get_unchecked(i).mul(&u_rhs))
                        .sub(&z.get_unchecked(i).mul(&z_rhs));

                    yj = yj.add(&(*aij).conj().mul(u.get_unchecked(i)));
                }
            }

            yj
        };
        y[j] = yj.add(&a_row[j].conj());
        a_row[j] = a_row[j].sub(&y[j].conj().mul(&tl_inv));

        let rhs = a_row[j].conj();

        if coe::is_same::<f64, T>() {
            unsafe {
                bidiag_fused_op_step1_f64(
                    arch,
                    slice::from_raw_parts_mut(z_tmp.rb_mut().as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(a_next.rb().ptr_at(0, j), nrows).coerce(),
                    coe::coerce_static(rhs),
                );
            }
        } else if coe::is_same::<c64, T>() {
            unsafe {
                bidiag_fused_op_step1_c64(
                    arch,
                    slice::from_raw_parts_mut(z_tmp.rb_mut().as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(a_next.rb().ptr_at(0, j), nrows).coerce(),
                    coe::coerce_static(rhs),
                );
            }
        } else if coe::is_same::<f32, T>() {
            unsafe {
                bidiag_fused_op_step1_f32(
                    arch,
                    slice::from_raw_parts_mut(z_tmp.rb_mut().as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(a_next.rb().ptr_at(0, j), nrows).coerce(),
                    coe::coerce_static(rhs),
                );
            }
        } else if coe::is_same::<c32, T>() {
            unsafe {
                bidiag_fused_op_step1_c32(
                    arch,
                    slice::from_raw_parts_mut(z_tmp.rb_mut().as_ptr(), nrows).coerce(),
                    slice::from_raw_parts(a_next.rb().ptr_at(0, j), nrows).coerce(),
                    coe::coerce_static(rhs),
                );
            }
        } else {
            for i in 0..nrows {
                unsafe {
                    let zi = z_tmp.rb_mut().ptr_in_bounds_at_unchecked(i);
                    let aij = a_next.rb().get_unchecked(i, j).clone();
                    *zi = (*zi).add(&aij.mul(&rhs));
                }
            }
        }
    }
}

fn bidiag_fused_op<T: ComplexField>(
    k: usize,
    m: usize,
    n: usize,
    tl_prev: T,
    tl: T,
    tr: T,
    mut z_tmp: MatMut<'_, T>,
    a_left: MatMut<'_, T>,
    a_top: MatMut<'_, T>,
    mut a_next: MatMut<'_, T>,
    mut y: MatMut<'_, T>,
    parallelism: Parallelism,
    mut z: MatMut<'_, T>,
    u: MatRef<'_, T>,
    mut a_row: MatMut<'_, T>,
) {
    let parallelism = if m * n < 128 * 128 {
        Parallelism::None
    } else {
        parallelism
    };
    if k > 0 {
        if a_next.row_stride() == 1 {
            let arch = pulp::Arch::new();

            let n_threads = match parallelism {
                Parallelism::None => 1,
                Parallelism::Rayon(n_threads) => {
                    if n_threads == 0 {
                        rayon::current_num_threads()
                    } else {
                        n_threads
                    }
                }
            };

            let u_prev = a_left.rb().submatrix(k + 1, k - 1, m - 1, 1).col(0);
            let v_prev = a_top.rb().submatrix(k - 1, 1, 1, n - 1).row(0);

            let tl_prev_inv = tl_prev.inv();
            let tr_prev_inv = tr.inv();
            let tl_inv = tl.inv();

            fancy_assert!(a_next.row_stride() == 1);
            fancy_assert!(u_prev.row_stride() == 1);
            fancy_assert!(u.row_stride() == 1);
            fancy_assert!(z.row_stride() == 1);

            match n_threads {
                1 => {
                    bidiag_fused_op_process_batch(
                        arch,
                        z_tmp.rb_mut().col(0),
                        a_next,
                        a_row.row(0),
                        u.col(0),
                        u_prev,
                        v_prev,
                        y.col(0),
                        z.rb().col(0),
                        tl_prev_inv,
                        tr_prev_inv,
                        tl_inv,
                    );
                }
                n_threads => {
                    use rayon::prelude::*;

                    z_tmp
                        .rb_mut()
                        .into_par_col_chunks(n_threads)
                        .zip_eq(a_next.into_par_col_chunks(n_threads))
                        .zip_eq(a_row.into_par_col_chunks(n_threads))
                        .zip_eq(y.into_par_row_chunks(n_threads))
                        .zip_eq(v_prev.as_2d().into_par_col_chunks(n_threads))
                        .for_each(
                            |(((((_, z_tmp), (_, a_next)), (_, a_row)), (_, y)), (_, v_prev))| {
                                bidiag_fused_op_process_batch(
                                    arch,
                                    z_tmp.col(0),
                                    a_next,
                                    a_row.row(0),
                                    u.col(0),
                                    u_prev,
                                    v_prev.row(0),
                                    y.col(0),
                                    z.rb().col(0),
                                    tl_prev_inv.clone(),
                                    tr_prev_inv.clone(),
                                    tl_inv.clone(),
                                );
                            },
                        );
                }
            }

            let mut idx = 0;
            let mut first_init = true;
            while idx < n_threads {
                let bs = 4.min(n_threads - idx);
                let mut z_block = z_tmp.rb_mut().submatrix(0, idx, m - 1, bs).into_col_iter();

                match bs {
                    1 => {
                        let z0 = z_block.next().unwrap();
                        if first_init {
                            zip!(z.rb_mut().col(0), z0).for_each(|z, z0| {
                                *z = z0.clone();
                                *z0 = T::zero();
                            });
                        } else {
                            zip!(z.rb_mut().col(0), z0).for_each(|z, z0| {
                                *z = (*z).add(z0);
                                *z0 = T::zero();
                            });
                        }
                    }
                    2 => {
                        let z0 = z_block.next().unwrap();
                        let z1 = z_block.next().unwrap();
                        if first_init {
                            zip!(z.rb_mut().col(0), z0, z1).for_each(|z, z0, z1| {
                                *z = (*z0).add(z1);
                                *z0 = T::zero();
                                *z1 = T::zero();
                            });
                        } else {
                            zip!(z.rb_mut().col(0), z0, z1).for_each(|z, z0, z1| {
                                *z = (*z).add(z0).add(z1);
                                *z0 = T::zero();
                                *z1 = T::zero();
                            });
                        }
                    }
                    3 => {
                        let z0 = z_block.next().unwrap();
                        let z1 = z_block.next().unwrap();
                        let z2 = z_block.next().unwrap();
                        if first_init {
                            zip!(z.rb_mut().col(0), z0, z1, z2).for_each(|z, z0, z1, z2| {
                                *z = (*z0).add(z1).add(z2);
                                *z0 = T::zero();
                                *z1 = T::zero();
                                *z2 = T::zero();
                            });
                        } else {
                            zip!(z.rb_mut().col(0), z0, z1, z2).for_each(|z, z0, z1, z2| {
                                *z = ((*z).add(z0)).add(&(*z1).add(z2));
                                *z0 = T::zero();
                                *z1 = T::zero();
                                *z2 = T::zero();
                            });
                        }
                    }
                    4 => {
                        let z0 = z_block.next().unwrap();
                        let z1 = z_block.next().unwrap();
                        let z2 = z_block.next().unwrap();
                        let z3 = z_block.next().unwrap();
                        if first_init {
                            zip!(z.rb_mut().col(0), z0, z1, z2, z3).for_each(
                                |z, z0, z1, z2, z3| {
                                    *z = ((*z0).add(z1)).add(&(*z2).add(z3));
                                    *z0 = T::zero();
                                    *z1 = T::zero();
                                    *z2 = T::zero();
                                    *z3 = T::zero();
                                },
                            );
                        } else {
                            zip!(z.rb_mut().col(0), z0, z1, z2, z3).for_each(
                                |z, z0, z1, z2, z3| {
                                    *z = (*z).add(&((*z0).add(z1)).add(&(*z2).add(z3)));
                                    *z0 = T::zero();
                                    *z1 = T::zero();
                                    *z2 = T::zero();
                                    *z3 = T::zero();
                                },
                            );
                        }
                    }
                    _ => unreachable!(),
                }
                idx += bs;
                first_init = false;
            }
        } else {
            let u_prev = a_left.rb().submatrix(k + 1, k - 1, m - 1, 1);
            let v_prev = a_top.rb().submatrix(k - 1, 1, 1, n - 1);
            matmul(
                a_next.rb_mut(),
                Conj::No,
                u_prev,
                Conj::No,
                y.rb().transpose(),
                Conj::Yes,
                Some(T::one()),
                tl_prev.inv().neg(),
                parallelism,
            );
            matmul(
                a_next.rb_mut(),
                Conj::No,
                z.rb(),
                Conj::No,
                v_prev,
                Conj::Yes,
                Some(T::one()),
                tr.inv().neg(),
                parallelism,
            );

            matmul(
                y.rb_mut(),
                Conj::No,
                a_next.rb().transpose(),
                Conj::Yes,
                u,
                Conj::No,
                None,
                T::one(),
                parallelism,
            );
            zip!(y.rb_mut(), a_row.rb().transpose())
                .for_each(|dst, src| *dst = (*dst).add(&(*src).conj()));
            let tl_inv = tl.inv();
            zip!(a_row.rb_mut(), y.rb().transpose())
                .for_each(|dst, src| *dst = (*dst).sub(&(*src).conj().mul(&tl_inv)));
            matmul(
                z.rb_mut(),
                Conj::No,
                a_next.rb(),
                Conj::No,
                a_row.rb().transpose(),
                Conj::Yes,
                None,
                T::one(),
                parallelism,
            );
        }
    } else {
        matmul(
            y.rb_mut(),
            Conj::No,
            a_next.rb().transpose(),
            Conj::Yes,
            u,
            Conj::No,
            None,
            T::one(),
            parallelism,
        );
        zip!(y.rb_mut(), a_row.rb().transpose())
            .for_each(|dst, src| *dst = (*dst).add(&(*src).conj()));
        let tl_inv = tl.inv();
        zip!(a_row.rb_mut(), y.rb().transpose())
            .for_each(|dst, src| *dst = (*dst).sub(&(*src).conj().mul(&tl_inv)));

        matmul(
            z.rb_mut(),
            Conj::No,
            a_next.rb(),
            Conj::No,
            a_row.rb().transpose(),
            Conj::Yes,
            None,
            T::one(),
            parallelism,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_approx_eq::assert_approx_eq;
    use faer_core::{
        c64,
        householder::{
            apply_block_householder_sequence_on_the_right_in_place,
            apply_block_householder_sequence_transpose_on_the_left_in_place,
            apply_block_householder_sequence_transpose_on_the_left_in_place_req,
            apply_block_householder_sequence_transpose_on_the_right_in_place_req,
        },
        Mat,
    };

    macro_rules! make_stack {
        ($req: expr $(,)?) => {
            ::dyn_stack::DynStack::new(&mut ::dyn_stack::GlobalMemBuffer::new($req.unwrap()))
        };
    }

    #[test]
    fn bidiag_f64() {
        let mat = Mat::with_dims(|_, _| rand::random::<f64>(), 15, 10);

        let m = mat.nrows();
        let n = mat.ncols();

        let mut bid = mat.clone();
        let mut tau_left = Mat::zeros(n, 1);
        let mut tau_right = Mat::zeros(n - 1, 1);

        bidiagonalize_in_place(
            bid.as_mut(),
            tau_left.as_mut().col(0),
            tau_right.as_mut().col(0),
            Parallelism::None,
            make_stack!(bidiagonalize_in_place_req::<f64>(m, n, Parallelism::None)),
        );

        let mut copy = mat.clone();
        apply_block_householder_sequence_transpose_on_the_left_in_place(
            bid.as_ref(),
            tau_left.as_ref().transpose(),
            Conj::No,
            copy.as_mut(),
            Conj::No,
            Parallelism::None,
            make_stack!(
                apply_block_householder_sequence_transpose_on_the_left_in_place_req::<f64>(m, 1, n),
            ),
        );

        apply_block_householder_sequence_on_the_right_in_place(
            bid.as_ref().submatrix(0, 1, m, n - 1).transpose(),
            tau_right.as_ref().transpose(),
            Conj::No,
            copy.as_mut().submatrix(0, 1, m, n - 1),
            Conj::No,
            Parallelism::None,
            make_stack!(
                apply_block_householder_sequence_transpose_on_the_right_in_place_req::<f64>(
                    n - 1,
                    1,
                    m,
                )
            ),
        );

        for j in 0..n {
            for i in (0..j.saturating_sub(1)).chain(j + 1..m) {
                assert_approx_eq!(copy[(i, j)], 0.0);
            }
        }
    }

    #[test]
    fn bidiag_c64() {
        let mat = Mat::with_dims(|_, _| c64::new(rand::random(), rand::random()), 15, 10);

        let m = mat.nrows();
        let n = mat.ncols();

        let mut bid = mat.clone();
        let mut tau_left = Mat::zeros(n, 1);
        let mut tau_right = Mat::zeros(n - 1, 1);

        bidiagonalize_in_place(
            bid.as_mut(),
            tau_left.as_mut().col(0),
            tau_right.as_mut().col(0),
            Parallelism::Rayon(0),
            make_stack!(bidiagonalize_in_place_req::<c64>(
                m,
                n,
                Parallelism::Rayon(0)
            )),
        );

        let mut copy = mat.clone();
        apply_block_householder_sequence_transpose_on_the_left_in_place(
            bid.as_ref(),
            tau_left.as_ref().transpose(),
            Conj::Yes,
            copy.as_mut(),
            Conj::No,
            Parallelism::Rayon(0),
            make_stack!(
                apply_block_householder_sequence_transpose_on_the_left_in_place_req::<c64>(m, 1, n),
            ),
        );

        apply_block_householder_sequence_on_the_right_in_place(
            bid.as_ref().submatrix(0, 1, m, n - 1).transpose(),
            tau_right.as_ref().transpose(),
            Conj::No,
            copy.as_mut().submatrix(0, 1, m, n - 1),
            Conj::No,
            Parallelism::Rayon(0),
            make_stack!(
                apply_block_householder_sequence_transpose_on_the_right_in_place_req::<c64>(
                    n - 1,
                    1,
                    m,
                )
            ),
        );

        for j in 0..n {
            for i in (0..j.saturating_sub(1)).chain(j + 1..m) {
                assert_approx_eq!(copy[(i, j)], c64::zero());
            }
        }
    }
}
