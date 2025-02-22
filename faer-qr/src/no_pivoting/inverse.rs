use assert2::assert as fancy_assert;
use dyn_stack::{DynStack, SizeOverflow, StackReq};
use faer_core::{
    householder::apply_block_householder_sequence_transpose_on_the_right_in_place,
    inverse::invert_upper_triangular, temp_mat_req, temp_mat_uninit, zip, ComplexField, Conj,
    MatMut, MatRef, Parallelism,
};
use reborrow::*;

/// Computes the inverse of a matrix, given its QR decomposition,
/// and stores the result in `dst`.
///
/// # Panics
///
/// - Panics if `qr_factors` is not a square matrix.
/// - Panics if the number of columns of `householder_factor` isn't the same as the minimum of the
/// number of rows and the number of columns of `qr_factors`.
/// - Panics if the block size is zero.
/// - Panics if `dst` doesn't have the same shape as `qr_factors`.
/// - Panics if the provided memory in `stack` is insufficient.
#[track_caller]
pub fn invert<T: ComplexField>(
    dst: MatMut<'_, T>,
    qr_factors: MatRef<'_, T>,
    householder_factor: MatRef<'_, T>,
    parallelism: Parallelism,
    stack: DynStack<'_>,
) {
    fancy_assert!(qr_factors.nrows() == qr_factors.ncols());
    fancy_assert!((dst.nrows(), dst.ncols()) == (qr_factors.nrows(), qr_factors.ncols()));
    fancy_assert!(householder_factor.ncols() == usize::min(qr_factors.nrows(), qr_factors.ncols()));
    fancy_assert!(householder_factor.nrows() > 0);

    let mut dst = dst;

    // invert R
    invert_upper_triangular(dst.rb_mut(), qr_factors, Conj::No, parallelism);

    // zero bottom part
    dst.rb_mut()
        .cwise()
        .for_each_triangular_lower(faer_core::zip::Diag::Skip, |dst| *dst = T::zero());

    apply_block_householder_sequence_transpose_on_the_right_in_place(
        qr_factors,
        householder_factor,
        Conj::Yes,
        dst.rb_mut(),
        Conj::No,
        parallelism,
        stack,
    );
}

/// Computes the inverse of a matrix, given its QR decomposition,
/// and stores the result in `qr_factors`.
///
/// # Panics
///
/// - Panics if `qr_factors` is not a square matrix.
/// - Panics if the number of columns of `householder_factor` isn't the same as the minimum of the
/// number of rows and the number of columns of `qr_factors`.
/// - Panics if the block size is zero.
/// - Panics if the provided memory in `stack` is insufficient.
#[track_caller]
pub fn invert_in_place<T: ComplexField>(
    qr_factors: MatMut<'_, T>,
    householder_factor: MatRef<'_, T>,
    parallelism: Parallelism,
    stack: DynStack<'_>,
) {
    let (mut dst, stack) =
        unsafe { temp_mat_uninit::<T>(qr_factors.nrows(), qr_factors.ncols(), stack) };
    let mut dst = dst.as_mut();

    invert(
        dst.rb_mut(),
        qr_factors.rb(),
        householder_factor,
        parallelism,
        stack,
    );

    zip!(qr_factors, dst.rb()).for_each(|dst, src| *dst = src.clone());
}

/// Computes the size and alignment of required workspace for computing the inverse of a
/// matrix out of place, given its QR decomposition.
pub fn invert_req<T: 'static>(
    qr_nrows: usize,
    qr_ncols: usize,
    blocksize: usize,
    parallelism: Parallelism,
) -> Result<StackReq, SizeOverflow> {
    let _ = qr_nrows;
    let _ = parallelism;
    temp_mat_req::<T>(blocksize, qr_ncols)
}

/// Computes the size and alignment of required workspace for computing the inverse of a
/// matrix in place, given its QR decomposition.
pub fn invert_in_place_req<T: 'static>(
    qr_nrows: usize,
    qr_ncols: usize,
    blocksize: usize,
    parallelism: Parallelism,
) -> Result<StackReq, SizeOverflow> {
    StackReq::try_all_of([
        temp_mat_req::<T>(qr_nrows, qr_ncols)?,
        invert_req::<T>(qr_nrows, qr_ncols, blocksize, parallelism)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::no_pivoting::compute::{qr_in_place, qr_in_place_req, recommended_blocksize};
    use assert_approx_eq::assert_approx_eq;
    use faer_core::{c64, Mat};
    use rand::prelude::*;
    use std::cell::RefCell;

    macro_rules! make_stack {
        ($req: expr) => {
            ::dyn_stack::DynStack::new(&mut ::dyn_stack::GlobalMemBuffer::new($req.unwrap()))
        };
    }

    type T = c64;

    thread_local! {
        static RNG: RefCell<StdRng> = RefCell::new(StdRng::seed_from_u64(0));
    }

    fn random_value() -> T {
        RNG.with(|rng| {
            let mut rng = rng.borrow_mut();
            let rng = &mut *rng;
            T::new(rng.gen(), rng.gen())
        })
    }

    #[test]
    fn test_invert() {
        for n in [31, 32, 48, 65] {
            let mat = Mat::with_dims(|_, _| random_value(), n, n);
            let blocksize = recommended_blocksize::<T>(n, n);
            let mut qr = mat.clone();
            let mut householder_factor = Mat::zeros(blocksize, n);

            let parallelism = faer_core::Parallelism::Rayon(0);

            qr_in_place(
                qr.as_mut(),
                householder_factor.as_mut(),
                parallelism,
                make_stack!(qr_in_place_req::<T>(
                    n,
                    n,
                    blocksize,
                    parallelism,
                    Default::default()
                )),
                Default::default(),
            );

            let mut inv = Mat::zeros(n, n);
            invert(
                inv.as_mut(),
                qr.as_ref(),
                householder_factor.as_ref(),
                parallelism,
                make_stack!(invert_req::<T>(n, n, blocksize, parallelism)),
            );

            let eye = &inv * &mat;

            for i in 0..n {
                for j in 0..n {
                    let target = if i == j { T::one() } else { T::zero() };
                    assert_approx_eq!(eye[(i, j)], target);
                }
            }
        }
    }
}
