use assert2::{assert as fancy_assert, debug_assert as fancy_debug_assert};
use coe::Coerce;
use core::slice;
use dyn_stack::{DynStack, SizeOverflow, StackReq};
use faer_core::{
    c32, c64,
    householder::upgrade_householder_factor,
    mul::dot,
    permutation::{swap_cols, PermutationMut},
    ColMut, ColRef, ComplexField, MatMut, Parallelism,
};
use pulp::{as_arrays, as_arrays_mut, Simd};
use reborrow::*;

pub use crate::no_pivoting::compute::recommended_blocksize;

// a += k * b
//
// returns ||a||²
fn update_and_norm2_f64(arch: pulp::Arch, a: &mut [f64], b: &[f64], k: f64) -> f64 {
    struct Impl<'a> {
        a: &'a mut [f64],
        b: &'a [f64],
        k: f64,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = f64;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { a, b, k } = self;
            let mut acc0 = simd.f64s_splat(0.0);
            let mut acc1 = simd.f64s_splat(0.0);
            let mut acc2 = simd.f64s_splat(0.0);
            let mut acc3 = simd.f64s_splat(0.0);
            let mut acc4 = simd.f64s_splat(0.0);
            let mut acc5 = simd.f64s_splat(0.0);
            let mut acc6 = simd.f64s_splat(0.0);
            let mut acc7 = simd.f64s_splat(0.0);

            let (a, a_rem) = S::f64s_as_mut_simd(a);
            let (b, b_rem) = S::f64s_as_simd(b);

            let (a, a_remv) = as_arrays_mut::<8, _>(a);
            let (b, b_remv) = as_arrays::<8, _>(b);

            let vk = simd.f64s_splat(k);

            for (a, b) in a.iter_mut().zip(b.iter()) {
                a[0] = simd.f64s_mul_adde(vk, b[0], a[0]);
                acc0 = simd.f64s_mul_adde(a[0], a[0], acc0);

                a[1] = simd.f64s_mul_adde(vk, b[1], a[1]);
                acc1 = simd.f64s_mul_adde(a[1], a[1], acc1);

                a[2] = simd.f64s_mul_adde(vk, b[2], a[2]);
                acc2 = simd.f64s_mul_adde(a[2], a[2], acc2);

                a[3] = simd.f64s_mul_adde(vk, b[3], a[3]);
                acc3 = simd.f64s_mul_adde(a[3], a[3], acc3);

                a[4] = simd.f64s_mul_adde(vk, b[4], a[4]);
                acc4 = simd.f64s_mul_adde(a[4], a[4], acc4);

                a[5] = simd.f64s_mul_adde(vk, b[5], a[5]);
                acc5 = simd.f64s_mul_adde(a[5], a[5], acc5);

                a[6] = simd.f64s_mul_adde(vk, b[6], a[6]);
                acc6 = simd.f64s_mul_adde(a[6], a[6], acc6);

                a[7] = simd.f64s_mul_adde(vk, b[7], a[7]);
                acc7 = simd.f64s_mul_adde(a[7], a[7], acc7);
            }

            for (a, b) in a_remv.iter_mut().zip(b_remv.iter()) {
                *a = simd.f64s_mul_adde(vk, *b, *a);
                acc0 = simd.f64s_mul_adde(*a, *a, acc0);
            }

            acc0 = simd.f64s_add(acc0, acc1);
            acc2 = simd.f64s_add(acc2, acc3);
            acc4 = simd.f64s_add(acc4, acc5);
            acc6 = simd.f64s_add(acc6, acc7);

            acc0 = simd.f64s_add(acc0, acc2);
            acc4 = simd.f64s_add(acc4, acc6);

            acc0 = simd.f64s_add(acc0, acc4);

            let mut acc = simd.f64s_reduce_sum(acc0);

            for (a, b) in a_rem.iter_mut().zip(b_rem.iter()) {
                *a = f64::mul_add(k, *b, *a);
                acc = f64::mul_add(*a, *a, acc);
            }

            acc
        }
    }
    arch.dispatch(Impl { a, b, k })
}

fn update_and_norm2_c64(arch: pulp::Arch, a: &mut [c64], b: &[c64], k: c64) -> f64 {
    struct Impl<'a> {
        a: &'a mut [c64],
        b: &'a [c64],
        k: c64,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = f64;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { a, b, k } = self;
            let mut acc0 = simd.f64s_splat(0.0);
            let mut acc1 = simd.f64s_splat(0.0);
            let mut acc2 = simd.f64s_splat(0.0);
            let mut acc3 = simd.f64s_splat(0.0);
            let mut acc4 = simd.f64s_splat(0.0);
            let mut acc5 = simd.f64s_splat(0.0);
            let mut acc6 = simd.f64s_splat(0.0);
            let mut acc7 = simd.f64s_splat(0.0);

            let (a, a_rem) = S::c64s_as_mut_simd(a);
            let (b, b_rem) = S::c64s_as_simd(b);

            let (a, a_remv) = as_arrays_mut::<8, _>(a);
            let (b, b_remv) = as_arrays::<8, _>(b);

            let vk = simd.c64s_splat(k);

            #[inline(always)]
            fn accumulate<S: Simd>(simd: S, acc: S::f64s, a: S::c64s) -> S::f64s {
                if coe::is_same::<S, pulp::Scalar>() {
                    let norm2: c64 = bytemuck::cast(simd.c64s_abs2(a));
                    bytemuck::cast(norm2)
                } else {
                    simd.f64s_mul_adde(bytemuck::cast(a), bytemuck::cast(a), acc)
                }
            }

            for (a, b) in a.iter_mut().zip(b.iter()) {
                a[0] = simd.c64s_mul_adde(vk, b[0], a[0]);
                acc0 = accumulate(simd, acc0, a[0]);

                a[1] = simd.c64s_mul_adde(vk, b[1], a[1]);
                acc1 = accumulate(simd, acc1, a[1]);

                a[2] = simd.c64s_mul_adde(vk, b[2], a[2]);
                acc2 = accumulate(simd, acc2, a[2]);

                a[3] = simd.c64s_mul_adde(vk, b[3], a[3]);
                acc3 = accumulate(simd, acc3, a[3]);

                a[4] = simd.c64s_mul_adde(vk, b[4], a[4]);
                acc4 = accumulate(simd, acc4, a[4]);

                a[5] = simd.c64s_mul_adde(vk, b[5], a[5]);
                acc5 = accumulate(simd, acc5, a[5]);

                a[6] = simd.c64s_mul_adde(vk, b[6], a[6]);
                acc6 = accumulate(simd, acc6, a[6]);

                a[7] = simd.c64s_mul_adde(vk, b[7], a[7]);
                acc7 = accumulate(simd, acc7, a[7]);
            }

            for (a, b) in a_remv.iter_mut().zip(b_remv.iter()) {
                *a = simd.c64s_mul_adde(vk, *b, *a);
                acc0 = accumulate(simd, acc0, *a);
            }

            acc0 = simd.f64s_add(acc0, acc1);
            acc2 = simd.f64s_add(acc2, acc3);
            acc4 = simd.f64s_add(acc4, acc5);
            acc6 = simd.f64s_add(acc6, acc7);

            acc0 = simd.f64s_add(acc0, acc2);
            acc4 = simd.f64s_add(acc4, acc6);

            acc0 = simd.f64s_add(acc0, acc4);

            let mut acc = simd.f64s_reduce_sum(acc0);

            for (a, b) in a_rem.iter_mut().zip(b_rem.iter()) {
                *a = k * *b + *a;
                acc = a.re * a.re + a.im * a.im;
            }

            acc
        }
    }
    arch.dispatch(Impl { a, b, k })
}

// a += k * b
//
// returns ||a||²
fn update_and_norm2_f32(arch: pulp::Arch, a: &mut [f32], b: &[f32], k: f32) -> f32 {
    struct Impl<'a> {
        a: &'a mut [f32],
        b: &'a [f32],
        k: f32,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = f32;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { a, b, k } = self;
            let mut acc0 = simd.f32s_splat(0.0);
            let mut acc1 = simd.f32s_splat(0.0);
            let mut acc2 = simd.f32s_splat(0.0);
            let mut acc3 = simd.f32s_splat(0.0);
            let mut acc4 = simd.f32s_splat(0.0);
            let mut acc5 = simd.f32s_splat(0.0);
            let mut acc6 = simd.f32s_splat(0.0);
            let mut acc7 = simd.f32s_splat(0.0);

            let (a, a_rem) = S::f32s_as_mut_simd(a);
            let (b, b_rem) = S::f32s_as_simd(b);

            let (a, a_remv) = as_arrays_mut::<8, _>(a);
            let (b, b_remv) = as_arrays::<8, _>(b);

            let vk = simd.f32s_splat(k);

            for (a, b) in a.iter_mut().zip(b.iter()) {
                a[0] = simd.f32s_mul_adde(vk, b[0], a[0]);
                acc0 = simd.f32s_mul_adde(a[0], a[0], acc0);

                a[1] = simd.f32s_mul_adde(vk, b[1], a[1]);
                acc1 = simd.f32s_mul_adde(a[1], a[1], acc1);

                a[2] = simd.f32s_mul_adde(vk, b[2], a[2]);
                acc2 = simd.f32s_mul_adde(a[2], a[2], acc2);

                a[3] = simd.f32s_mul_adde(vk, b[3], a[3]);
                acc3 = simd.f32s_mul_adde(a[3], a[3], acc3);

                a[4] = simd.f32s_mul_adde(vk, b[4], a[4]);
                acc4 = simd.f32s_mul_adde(a[4], a[4], acc4);

                a[5] = simd.f32s_mul_adde(vk, b[5], a[5]);
                acc5 = simd.f32s_mul_adde(a[5], a[5], acc5);

                a[6] = simd.f32s_mul_adde(vk, b[6], a[6]);
                acc6 = simd.f32s_mul_adde(a[6], a[6], acc6);

                a[7] = simd.f32s_mul_adde(vk, b[7], a[7]);
                acc7 = simd.f32s_mul_adde(a[7], a[7], acc7);
            }

            for (a, b) in a_remv.iter_mut().zip(b_remv.iter()) {
                *a = simd.f32s_mul_adde(vk, *b, *a);
                acc0 = simd.f32s_mul_adde(*a, *a, acc0);
            }

            acc0 = simd.f32s_add(acc0, acc1);
            acc2 = simd.f32s_add(acc2, acc3);
            acc4 = simd.f32s_add(acc4, acc5);
            acc6 = simd.f32s_add(acc6, acc7);

            acc0 = simd.f32s_add(acc0, acc2);
            acc4 = simd.f32s_add(acc4, acc6);

            acc0 = simd.f32s_add(acc0, acc4);

            let mut acc = simd.f32s_reduce_sum(acc0);

            for (a, b) in a_rem.iter_mut().zip(b_rem.iter()) {
                *a = f32::mul_add(k, *b, *a);
                acc = f32::mul_add(*a, *a, acc);
            }

            acc
        }
    }
    arch.dispatch(Impl { a, b, k })
}

fn update_and_norm2_c32(arch: pulp::Arch, a: &mut [c32], b: &[c32], k: c32) -> f32 {
    struct Impl<'a> {
        a: &'a mut [c32],
        b: &'a [c32],
        k: c32,
    }
    impl pulp::WithSimd for Impl<'_> {
        type Output = f32;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let Self { a, b, k } = self;
            let mut acc0 = simd.f32s_splat(0.0);
            let mut acc1 = simd.f32s_splat(0.0);
            let mut acc2 = simd.f32s_splat(0.0);
            let mut acc3 = simd.f32s_splat(0.0);
            let mut acc4 = simd.f32s_splat(0.0);
            let mut acc5 = simd.f32s_splat(0.0);
            let mut acc6 = simd.f32s_splat(0.0);
            let mut acc7 = simd.f32s_splat(0.0);

            let (a, a_rem) = S::c32s_as_mut_simd(a);
            let (b, b_rem) = S::c32s_as_simd(b);

            let (a, a_remv) = as_arrays_mut::<8, _>(a);
            let (b, b_remv) = as_arrays::<8, _>(b);

            let vk = simd.c32s_splat(k);

            #[inline(always)]
            fn accumulate<S: Simd>(simd: S, acc: S::f32s, a: S::c32s) -> S::f32s {
                if coe::is_same::<S, pulp::Scalar>() {
                    let norm2: c32 = bytemuck::cast(simd.c32s_abs2(a));
                    bytemuck::cast(norm2)
                } else {
                    simd.f32s_mul_adde(bytemuck::cast(a), bytemuck::cast(a), acc)
                }
            }

            for (a, b) in a.iter_mut().zip(b.iter()) {
                a[0] = simd.c32s_mul_adde(vk, b[0], a[0]);
                acc0 = accumulate(simd, acc0, a[0]);

                a[1] = simd.c32s_mul_adde(vk, b[1], a[1]);
                acc1 = accumulate(simd, acc1, a[1]);

                a[2] = simd.c32s_mul_adde(vk, b[2], a[2]);
                acc2 = accumulate(simd, acc2, a[2]);

                a[3] = simd.c32s_mul_adde(vk, b[3], a[3]);
                acc3 = accumulate(simd, acc3, a[3]);

                a[4] = simd.c32s_mul_adde(vk, b[4], a[4]);
                acc4 = accumulate(simd, acc4, a[4]);

                a[5] = simd.c32s_mul_adde(vk, b[5], a[5]);
                acc5 = accumulate(simd, acc5, a[5]);

                a[6] = simd.c32s_mul_adde(vk, b[6], a[6]);
                acc6 = accumulate(simd, acc6, a[6]);

                a[7] = simd.c32s_mul_adde(vk, b[7], a[7]);
                acc7 = accumulate(simd, acc7, a[7]);
            }

            for (a, b) in a_remv.iter_mut().zip(b_remv.iter()) {
                *a = simd.c32s_mul_adde(vk, *b, *a);
                acc0 = accumulate(simd, acc0, *a);
            }

            acc0 = simd.f32s_add(acc0, acc1);
            acc2 = simd.f32s_add(acc2, acc3);
            acc4 = simd.f32s_add(acc4, acc5);
            acc6 = simd.f32s_add(acc6, acc7);

            acc0 = simd.f32s_add(acc0, acc2);
            acc4 = simd.f32s_add(acc4, acc6);

            acc0 = simd.f32s_add(acc0, acc4);

            let mut acc = simd.f32s_reduce_sum(acc0);

            for (a, b) in a_rem.iter_mut().zip(b_rem.iter()) {
                *a = k * *b + *a;
                acc = a.re * a.re + a.im * a.im;
            }

            acc
        }
    }
    arch.dispatch(Impl { a, b, k })
}

#[inline(always)]
fn norm2<T: ComplexField>(arch: pulp::Arch, a: ColRef<'_, T>) -> T::Real {
    dot(arch, a, a).real()
}

#[inline(always)]
fn update_and_norm2<T: ComplexField>(
    arch: pulp::Arch,
    mut a: ColMut<'_, T>,
    b: ColRef<'_, T>,
    k: T,
) -> T::Real {
    let colmajor = a.row_stride() == 1 && b.row_stride() == 1;
    if colmajor {
        let a_len = a.nrows();
        let b_len = b.nrows();
        let a = unsafe { slice::from_raw_parts_mut(a.rb_mut().as_ptr(), a_len) };
        let b = unsafe { slice::from_raw_parts(b.as_ptr(), b_len) };

        if coe::is_same::<f64, T>() {
            return coe::coerce_static(update_and_norm2_f64(
                arch,
                a.coerce(),
                b.coerce(),
                coe::coerce_static(k),
            ));
        }
        if coe::is_same::<c64, T>() {
            return coe::coerce_static(update_and_norm2_c64(
                arch,
                a.coerce(),
                b.coerce(),
                coe::coerce_static(k),
            ));
        }
        if coe::is_same::<f32, T>() {
            return coe::coerce_static(update_and_norm2_f32(
                arch,
                a.coerce(),
                b.coerce(),
                coe::coerce_static(k),
            ));
        }
        if coe::is_same::<c32, T>() {
            return coe::coerce_static(update_and_norm2_c32(
                arch,
                a.coerce(),
                b.coerce(),
                coe::coerce_static(k),
            ));
        }
    }

    let mut acc = T::Real::zero();
    for (a, b) in a.into_iter().zip(b.into_iter()) {
        *a = a.add(&k.mul(b));
        acc = acc.add(&((*a).conj().mul(a)).real());
    }

    acc
}

fn qr_in_place_colmajor<T: ComplexField>(
    arch: pulp::Arch,
    mut matrix: MatMut<'_, T>,
    mut householder_coeffs: ColMut<'_, T>,
    col_perm: &mut [usize],
    parallelism: Parallelism,
    disable_parallelism: fn(usize, usize) -> bool,
) -> usize {
    let m = matrix.nrows();
    let n = matrix.ncols();
    let size = m.min(n);

    fancy_debug_assert!(householder_coeffs.nrows() == size);

    let mut n_transpositions = 0;

    if size == 0 {
        return n_transpositions;
    }

    let mut biggest_col_idx = 0;
    let mut biggest_col_value = T::Real::zero();
    for j in 0..n {
        let col_value = norm2(arch, matrix.rb().col(j));
        if col_value > biggest_col_value {
            biggest_col_value = col_value;
            biggest_col_idx = j;
        }
    }

    for k in 0..size {
        let mut matrix_right = matrix.rb_mut().submatrix(0, k, m, n - k);

        col_perm.swap(k, k + biggest_col_idx);
        if biggest_col_idx > 0 {
            n_transpositions += 1;
            swap_cols(matrix_right.rb_mut(), 0, biggest_col_idx);
        }

        let mut matrix = matrix.rb_mut().submatrix(k, k, m - k, n - k);
        let m = matrix.nrows();
        let n = matrix.ncols();

        let (_, _, first_col, last_cols) = matrix.rb_mut().split_at(0, 1);
        let first_col = first_col.col(0);

        let (mut first_head, mut first_tail) = first_col.split_at(1);
        let tail_squared_norm = norm2(arch, first_tail.rb());
        let (tau, beta) = faer_core::householder::make_householder_in_place(
            Some(first_tail.rb_mut()),
            first_head[0].clone(),
            tail_squared_norm,
        );
        first_head[0] = beta;
        unsafe { *householder_coeffs.rb_mut().ptr_in_bounds_at(k) = tau.clone() };
        let tau_inv = tau.inv();

        let first_tail = first_tail.rb();

        if n == 0 {
            return n_transpositions;
        }

        let extra_parallelism = if disable_parallelism(m, n) {
            Parallelism::None
        } else {
            parallelism
        };

        match extra_parallelism {
            Parallelism::Rayon(n_threads) => {
                use rayon::prelude::*;
                let n_threads = if n_threads > 0 {
                    n_threads
                } else {
                    rayon::current_num_threads()
                };

                let mut biggest_col = vec![(T::Real::zero(), 0_usize); n_threads];

                last_cols
                    .into_par_col_chunks(n_threads)
                    .zip(biggest_col.par_iter_mut())
                    .for_each(
                        |((col_start, matrix), (biggest_col_value, biggest_col_idx))| {
                            process_cols(
                                arch,
                                matrix,
                                col_start,
                                first_tail,
                                tau_inv.clone(),
                                biggest_col_value,
                                biggest_col_idx,
                            );
                        },
                    );

                biggest_col_value = T::Real::zero();
                biggest_col_idx = 0;

                for (col_value, col_idx) in biggest_col {
                    if col_value > biggest_col_value {
                        biggest_col_value = col_value;
                        biggest_col_idx = col_idx;
                    }
                }
            }
            _ => {
                biggest_col_value = T::Real::zero();
                biggest_col_idx = 0;

                process_cols(
                    arch,
                    last_cols,
                    0,
                    first_tail,
                    tau_inv,
                    &mut biggest_col_value,
                    &mut biggest_col_idx,
                );
            }
        }
    }

    n_transpositions
}

fn process_cols<T: ComplexField>(
    arch: pulp::Arch,
    mut matrix: MatMut<'_, T>,
    offset: usize,
    first_tail: ColRef<'_, T>,
    tau_inv: T,
    biggest_col_value: &mut T::Real,
    biggest_col_idx: &mut usize,
) {
    for j in 0..matrix.ncols() {
        let (col_head, col_tail) = matrix.rb_mut().col(j).split_at(1);
        let col_head = col_head.get(0);

        let dot = col_head.add(&dot(arch, first_tail, col_tail.rb()));
        let k = (tau_inv.mul(&dot)).neg();
        *col_head = col_head.add(&k);

        let col_value = update_and_norm2(arch, col_tail, first_tail, k);
        if col_value > *biggest_col_value {
            *biggest_col_value = col_value;
            *biggest_col_idx = j + offset;
        }
    }
}

fn default_disable_parallelism(m: usize, n: usize) -> bool {
    let prod = m * n;
    prod < 192 * 256
}

#[derive(Default, Copy, Clone)]
#[non_exhaustive]
pub struct ColPivQrComputeParams {
    /// At which size the parallelism should be disabled. `None` to automatically determine this
    /// threshold.
    pub disable_parallelism: Option<fn(nrows: usize, ncols: usize) -> bool>,
}

impl ColPivQrComputeParams {
    fn normalize(self) -> fn(usize, usize) -> bool {
        self.disable_parallelism
            .unwrap_or(default_disable_parallelism)
    }
}

/// Computes the size and alignment of required workspace for performing a QR decomposition
/// with column pivoting.
pub fn qr_in_place_req<T: 'static>(
    nrows: usize,
    ncols: usize,
    blocksize: usize,
    parallelism: Parallelism,
    params: ColPivQrComputeParams,
) -> Result<StackReq, SizeOverflow> {
    let _ = nrows;
    let _ = ncols;
    let _ = parallelism;
    let _ = blocksize;
    let _ = &params;
    Ok(StackReq::default())
}

/// Computes the QR decomposition with pivoting of a rectangular matrix $A$, into a unitary matrix
/// $Q$, represented as a block Householder sequence, and an upper trapezoidal matrix $R$, such
/// that $$AP^\top = QR.$$
///
/// The Householder bases of $Q$ are stored in the strictly lower trapezoidal part of `matrix` with
/// an implicit unit diagonal, and its upper triangular Householder factors are stored in
/// `householder_factor`, blockwise in chunks of `blocksize×blocksize`.
///
/// The block size is chosed as the number of rows of `householder_factor`.
///
/// # Output
///
/// - The number of transpositions that constitute the permutation.
/// - a structure representing the permutation $P$.
///
/// # Panics
///
/// - Panics if the number of columns of the householder factor is not equal to the minimum of the
/// number of rows and the number of columns of the input matrix.
/// - Panics if the block size is zero.
/// - Panics if the length of `col_perm` and `col_perm_inv` is not equal to the number of columns
/// of `matrix`.
/// - Panics if the provided memory in `stack` is insufficient.
pub fn qr_in_place<'out, T: ComplexField>(
    matrix: MatMut<'_, T>,
    householder_factor: MatMut<'_, T>,
    col_perm: &'out mut [usize],
    col_perm_inv: &'out mut [usize],
    parallelism: Parallelism,
    stack: DynStack<'_>,
    params: ColPivQrComputeParams,
) -> (usize, PermutationMut<'out>) {
    let _ = &stack;
    let disable_parallelism = params.normalize();
    let m = matrix.nrows();
    let n = matrix.ncols();

    fancy_assert!(col_perm.len() == n);
    fancy_assert!(col_perm_inv.len() == n);

    for (j, p) in col_perm.iter_mut().enumerate() {
        *p = j;
    }

    let mut householder_factor = householder_factor;
    let householder_coeffs = householder_factor.rb_mut().row(0).transpose();

    let mut matrix = matrix;

    let n_transpositions = qr_in_place_colmajor(
        pulp::Arch::new(),
        matrix.rb_mut(),
        householder_coeffs,
        col_perm,
        parallelism,
        disable_parallelism,
    );

    fn div_ceil(a: usize, b: usize) -> usize {
        let (div, rem) = (a / b, a % b);
        if rem == 0 {
            div
        } else {
            div + 1
        }
    }

    let blocksize = householder_factor.nrows();
    let size = householder_factor.ncols();
    let n_blocks = div_ceil(size, blocksize);

    let qr_factors = matrix.rb();

    let func = |idx: usize| {
        let j = idx * blocksize;
        let blocksize = blocksize.min(size - j);
        let mut householder =
            unsafe { householder_factor.rb().const_cast() }.submatrix(0, j, blocksize, blocksize);

        for i in 0..blocksize {
            let coeff = householder[(0, i)].clone();
            householder[(i, i)] = coeff;
        }

        let qr = qr_factors.submatrix(j, j, m - j, blocksize);

        upgrade_householder_factor(householder, qr, blocksize, 1, parallelism);
    };

    match parallelism {
        Parallelism::None => (0..n_blocks).for_each(func),
        Parallelism::Rayon(_) => {
            use rayon::prelude::*;
            (0..n_blocks).into_par_iter().for_each(func)
        }
    }

    for (j, &p) in col_perm.iter().enumerate() {
        col_perm_inv[p] = j;
    }

    (n_transpositions, unsafe {
        PermutationMut::new_unchecked(col_perm, col_perm_inv)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_approx_eq::assert_approx_eq;
    use faer_core::{
        c64,
        householder::{
            apply_block_householder_sequence_on_the_left_in_place,
            apply_block_householder_sequence_on_the_left_in_place_req,
        },
        mul::matmul,
        zip::Diag,
        Conj, Mat, MatRef,
    };
    use rand::random;

    macro_rules! make_stack {
        ($req: expr $(,)?) => {
            ::dyn_stack::DynStack::new(&mut ::dyn_stack::GlobalMemBuffer::new($req.unwrap()))
        };
    }

    fn reconstruct_factors<T: ComplexField>(
        qr_factors: MatRef<'_, T>,
        householder: MatRef<'_, T>,
    ) -> (Mat<T>, Mat<T>) {
        let m = qr_factors.nrows();
        let n = qr_factors.ncols();

        let mut q = Mat::zeros(m, m);
        let mut r = Mat::zeros(m, n);

        r.as_mut()
            .cwise()
            .zip(qr_factors)
            .for_each_triangular_upper(Diag::Include, |a, b| *a = b.clone());

        q.as_mut().diagonal().cwise().for_each(|a| *a = T::one());

        apply_block_householder_sequence_on_the_left_in_place(
            qr_factors,
            householder,
            Conj::No,
            q.as_mut(),
            Conj::No,
            Parallelism::Rayon(0),
            make_stack!(
                apply_block_householder_sequence_on_the_left_in_place_req::<T>(
                    m,
                    householder.nrows(),
                    m
                )
            ),
        );

        (q, r)
    }

    #[test]
    fn test_qr_f64() {
        for parallelism in [Parallelism::None, Parallelism::Rayon(8)] {
            for (m, n) in [(2, 2), (2, 4), (4, 2), (4, 4), (63, 63), (1024, 1024)] {
                let mut mat = Mat::<f64>::with_dims(|_, _| random(), m, n);
                let mat_orig = mat.clone();
                let size = m.min(n);
                let blocksize = 8;
                let mut householder = Mat::zeros(blocksize, size);
                let mut perm = vec![0; n];
                let mut perm_inv = vec![0; n];

                qr_in_place(
                    mat.as_mut(),
                    householder.as_mut(),
                    &mut perm,
                    &mut perm_inv,
                    parallelism,
                    make_stack!(qr_in_place_req::<f64>(
                        m,
                        n,
                        blocksize,
                        parallelism,
                        Default::default()
                    )),
                    Default::default(),
                );

                let (q, r) = reconstruct_factors(mat.as_ref(), householder.as_ref());
                let mut qr = Mat::zeros(m, n);
                matmul(
                    qr.as_mut(),
                    Conj::No,
                    q.as_ref(),
                    Conj::No,
                    r.as_ref(),
                    Conj::No,
                    None,
                    1.0,
                    Parallelism::Rayon(8),
                );

                for j in 0..n {
                    for i in 0..m {
                        assert_approx_eq!(qr[(i, j)], mat_orig[(i, perm[j])]);
                    }
                }
            }
        }
    }

    #[test]
    fn test_qr_c64() {
        for parallelism in [Parallelism::None, Parallelism::Rayon(8)] {
            for (m, n) in [(2, 2), (2, 4), (4, 2), (4, 4), (63, 63)] {
                let mut mat = Mat::<c64>::with_dims(|_, _| c64::new(random(), random()), m, n);
                let mat_orig = mat.clone();
                let size = m.min(n);
                let blocksize = 8;
                let mut householder = Mat::zeros(blocksize, size);
                let mut perm = vec![0; n];
                let mut perm_inv = vec![0; n];

                qr_in_place(
                    mat.as_mut(),
                    householder.as_mut(),
                    &mut perm,
                    &mut perm_inv,
                    parallelism,
                    make_stack!(qr_in_place_req::<c64>(
                        m,
                        n,
                        blocksize,
                        parallelism,
                        Default::default()
                    )),
                    Default::default(),
                );

                let (q, r) = reconstruct_factors(mat.as_ref(), householder.as_ref());
                let mut qr = Mat::zeros(m, n);
                let mut qhq = Mat::zeros(m, m);
                matmul(
                    qr.as_mut(),
                    Conj::No,
                    q.as_ref(),
                    Conj::No,
                    r.as_ref(),
                    Conj::No,
                    None,
                    c64::one(),
                    Parallelism::Rayon(8),
                );

                matmul(
                    qhq.as_mut(),
                    Conj::No,
                    q.as_ref().transpose(),
                    Conj::Yes,
                    q.as_ref(),
                    Conj::No,
                    None,
                    c64::one(),
                    Parallelism::Rayon(8),
                );

                for j in 0..n {
                    for i in 0..m {
                        assert_approx_eq!(qr[(i, j)], mat_orig[(i, perm[j])]);
                    }
                }
            }
        }
    }

    #[test]
    fn test_qr_f32() {
        for parallelism in [Parallelism::None, Parallelism::Rayon(8)] {
            for (m, n) in [(2, 2), (2, 4), (4, 2), (4, 4), (63, 63), (1024, 1024)] {
                let mut mat = Mat::<f32>::with_dims(|_, _| random(), m, n);
                let mat_orig = mat.clone();
                let size = m.min(n);
                let blocksize = 8;
                let mut householder = Mat::zeros(blocksize, size);
                let mut perm = vec![0; n];
                let mut perm_inv = vec![0; n];

                qr_in_place(
                    mat.as_mut(),
                    householder.as_mut(),
                    &mut perm,
                    &mut perm_inv,
                    parallelism,
                    make_stack!(qr_in_place_req::<f32>(
                        m,
                        n,
                        blocksize,
                        parallelism,
                        Default::default()
                    )),
                    Default::default(),
                );

                let (q, r) = reconstruct_factors(mat.as_ref(), householder.as_ref());
                let mut qr = Mat::zeros(m, n);
                matmul(
                    qr.as_mut(),
                    Conj::No,
                    q.as_ref(),
                    Conj::No,
                    r.as_ref(),
                    Conj::No,
                    None,
                    1.0,
                    Parallelism::Rayon(8),
                );

                for j in 0..n {
                    for i in 0..m {
                        assert_approx_eq!(qr[(i, j)], mat_orig[(i, perm[j])], 1e-4);
                    }
                }
            }
        }
    }

    #[test]
    fn test_qr_c32() {
        for parallelism in [Parallelism::None, Parallelism::Rayon(8)] {
            for (m, n) in [(2, 2), (2, 4), (4, 2), (4, 4), (63, 63)] {
                let mut mat = Mat::<c32>::with_dims(|_, _| c32::new(random(), random()), m, n);
                let mat_orig = mat.clone();
                let size = m.min(n);
                let blocksize = 8;
                let mut householder = Mat::zeros(blocksize, size);
                let mut perm = vec![0; n];
                let mut perm_inv = vec![0; n];

                qr_in_place(
                    mat.as_mut(),
                    householder.as_mut(),
                    &mut perm,
                    &mut perm_inv,
                    parallelism,
                    make_stack!(qr_in_place_req::<c32>(
                        m,
                        n,
                        blocksize,
                        parallelism,
                        Default::default()
                    )),
                    Default::default(),
                );

                let (q, r) = reconstruct_factors(mat.as_ref(), householder.as_ref());
                let mut qr = Mat::zeros(m, n);
                let mut qhq = Mat::zeros(m, m);
                matmul(
                    qr.as_mut(),
                    Conj::No,
                    q.as_ref(),
                    Conj::No,
                    r.as_ref(),
                    Conj::No,
                    None,
                    c32::one(),
                    Parallelism::Rayon(8),
                );

                matmul(
                    qhq.as_mut(),
                    Conj::No,
                    q.as_ref().transpose(),
                    Conj::Yes,
                    q.as_ref(),
                    Conj::No,
                    None,
                    c32::one(),
                    Parallelism::Rayon(8),
                );

                for j in 0..n {
                    for i in 0..m {
                        assert_approx_eq!(qr[(i, j)], mat_orig[(i, perm[j])], 1e-4);
                    }
                }
            }
        }
    }
}
