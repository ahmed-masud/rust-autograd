use super::*;
use crate::NdArray;
use std::slice;
use ndarray::IxDyn;

pub struct Conv2D {
    pub pad: usize,
    pub stride: usize,
    pub dilation: usize,
}

pub struct Conv2DFilterGrad {
    pub pad: usize,
    pub stride: usize,
    pub dilation: usize,
}

pub struct Conv2DWithCols {
    pub pad: usize,
    pub stride: usize,
    pub dilation: usize,
}

#[cfg(feature = "mkl")]
// inputs must be row-major matrices
fn fast_col_x_filter_kernel<F: Float>(
    cols: &[F], filter: &[F], y: &mut [F],
    xch: usize, ych: usize, yh: usize, yw: usize, kh: usize, kw: usize, batch_size: usize,
) {
    // params for blas gemm
    let m = ych as MklInt;
    let n = (yh * yw) as MklInt;
    let k = (xch * kh * kw) as MklInt;
    macro_rules! kernel_call_def { ($ty:ty, $f:ident) => {
        if crate::same_type::<$ty, F>() {
            const GROUP_COUNT: usize = 1;  // Fixed
            unsafe {
                $f(
                    CBLAS_ROW_MAJOR,
                    [CblasNoTrans; GROUP_COUNT].as_ptr(),
                    [CblasNoTrans; GROUP_COUNT].as_ptr(),
                    [m; GROUP_COUNT].as_ptr(),
                    [n; GROUP_COUNT].as_ptr(),
                    [k; GROUP_COUNT].as_ptr(),
                    [1.; GROUP_COUNT].as_ptr(),
                    vec![filter.as_ptr() as *const $ty; batch_size].as_ptr(), // a array
                    [k; GROUP_COUNT].as_ptr(),
                    get_batch_ptrs(batch_size, cols.as_ptr(), cols.len()).as_ptr(), // b array
                    [n; GROUP_COUNT].as_ptr(),
                    [0.; GROUP_COUNT].as_ptr(),
                    get_batch_ptrs_mut(batch_size, y.as_mut_ptr(), y.len()).as_mut_ptr(), // c array
                    [n ; GROUP_COUNT].as_ptr(),
                    GROUP_COUNT as MklInt,
                    [batch_size as MklInt; GROUP_COUNT].as_ptr()
                );
            }
        }
    }}
    kernel_call_def!(f32, cblas_sgemm_batch);
    kernel_call_def!(f64, cblas_dgemm_batch);
}

#[cfg(not(feature = "mkl"))]
fn slow_col_x_filter_kernel<F: Float>(
    cols: &[F], filter: &[F], y: &mut [F],
    xch: usize, ych: usize, yh: usize, yw: usize, kh: usize, kw: usize, batch_size: usize,
) {
    let size_per_batch_y = ych * yh * yw;
    let m = ych;
    let n = yh * yw;
    let k = xch * kh * kw;
    let (rsa, csa) = (k, 1);
    let (rsb, csb) = (n, 1);
    let (rsc, csc) = (n, 1);
    let size_per_batch_cols = xch * kw * kh * yh * yw;
    macro_rules! kernel_call_def { ($ty:ty, $f:ident) => {
        if crate::same_type::<$ty, F>() {
            (0..batch_size).into_par_iter().for_each(|i| {
                unsafe {
                    // for each batch
                    let cols_target: *const F = &cols[i * size_per_batch_cols];
                    let y_target: *mut F = mem::transmute(&y[i * size_per_batch_y]);
                    matrixmultiply::$f(
                        m,
                        k,
                        n,
                        1.,
                        filter.as_ptr() as *const $ty,
                        rsa as isize,
                        csa as isize,
                        cols_target as *const $ty,
                        rsb as isize,
                        csb as isize,
                        0.,
                        y_target as *mut $ty,
                        rsc as isize,
                        csc as isize,
                    );
                }
            });
        }
    }}
    kernel_call_def!(f32, sgemm);
    kernel_call_def!(f64, dgemm);
}

struct Conv2DParams {
    batch_size: usize, xch: usize, xh: usize, xw: usize, ych: usize, yh: usize, yw: usize, kh: usize, kw: usize
}

// Panics for invalid inputs
fn conv2d_extract_params<F: Float>(
    x: &NdArrayView<F>,
    w: &NdArrayView<F>,
    pad_h: usize,
    pad_w: usize,
    stride_h: usize,
    stride_w: usize,
    dilation_h: usize,
    dilation_w: usize,
) -> Conv2DParams {
    assert!(crate::same_type::<F, f32>() || crate::same_type::<F, f64>(), "autograd::conv2d: only f32 and f64 are supported.");
    // Extract size params
    let (batch_size, xch, xh, xw) = {
        let x_shape = x.shape();
        assert_eq!(
            x_shape.len(),
            4,
            "autograd: conv2d's lhs input must be 4D (got {:?})",
            x_shape
        );
        (x_shape[0], x_shape[1], x_shape[2], x_shape[3])
    };
    let (ych, kh, kw) = {
        let w_shape = w.shape();
        assert_eq!(
            w_shape.len(),
            4,
            "autograd::conv2d: filter must be 4D (got {:?})",
            w_shape
        );
        assert_eq!(
            xch, w_shape[1],
            "autograd::conv2d: input channel dim ({:?}) must match filter's second dim ({:?})",
            xch, w_shape[1]
        );
        (w_shape[0], w_shape[2], w_shape[3])
    };
    let yh = (xh + 2 * pad_h - (dilation_h * (kh - 1) + 1)) / stride_h + 1;
    let yw = (xw + 2 * pad_w - (dilation_w * (kw - 1) + 1)) / stride_w + 1;
    Conv2DParams{batch_size, xch, xh, xw, ych, yh, yw, kh, kw}
}

/// Returns: (conv result, im2col result)
#[allow(unused_assignments)]
fn conv2d_impl<F: Float>(
    x: &NdArrayView<F>,
    w: &NdArrayView<F>,
    pad_h: usize,
    pad_w: usize,
    stride_h: usize,
    stride_w: usize,
    dilation_h: usize,
    dilation_w: usize,
) -> (NdArray<F>, NdArray<F>)
{
    let Conv2DParams{batch_size, xch, xh, xw, ych, yh, yw, kh, kw} =
        conv2d_extract_params(x, w, pad_h, pad_w, stride_h, stride_w, dilation_h, dilation_w);

    let copied_x = ndarray_ext::copy_if_dirty(x);
    let copied_w = ndarray_ext::copy_if_dirty(w);

    // Prepare pointers to buffers
    let x_p = copied_x.map(|inner| inner.as_ptr()).unwrap_or(x.as_ptr());
    let w_p = copied_w.map(|inner| inner.as_ptr()).unwrap_or(w.as_ptr());
    let x_p = unsafe { slice::from_raw_parts(x_p, x.len()) };
    let w_p = unsafe { slice::from_raw_parts(w_p, w.len()) };

    // move vectors into ndarrays
    let cols = im2col_batch(x_p, batch_size, xch as i32, xh as i32, xw as i32,
                            kh as i32, kw as i32, pad_h as i32, pad_w as i32, stride_h as i32,
                            stride_w as i32, dilation_h as i32, dilation_w as i32,
    );

    unsafe {
        let size_per_batch_y = ych * yh * yw;
        let mut y = uninitialized_vec(batch_size * size_per_batch_y);
        let f;
        #[cfg(feature = "mkl")] { f = fast_col_x_filter_kernel; }
        #[cfg(not(feature = "mkl"))] { f = slow_col_x_filter_kernel; }
        f(cols.as_slice(), w_p, y.as_mut_slice(), xch, ych, yh, yw, kh, kw, batch_size);
        let y = NdArray::from_shape_vec_unchecked(IxDyn(&[batch_size, ych, yh, yw]), y);
        let cols = NdArray::from_shape_vec_unchecked(IxDyn(&[batch_size, xch, kw, kh, yh, yw]), cols);
        (y, cols)
    }
}

fn conv2d_with_cols_impl<F: Float>(
    cols: &NdArrayView<F>,
    w: &NdArrayView<F>,
) -> NdArray<F> {
    // Extract size params
    let cols_shape = cols.shape();
    let k_shape = w.shape();
    let (ych, xch, kh, kw) = { (k_shape[0], k_shape[1], k_shape[2], k_shape[3]) };
    let (yh, yw) = (cols_shape[4], cols_shape[5]);
    let batch_size = cols_shape[0];
    let size_per_batch_y = ych * yh * yw;

    // Prepare buffers
    let copied_w = ndarray_ext::copy_if_dirty(w);
    let w_slice = if let Some(ref inner) = copied_w {
        inner.as_slice().unwrap()
    } else {
        w.as_slice().unwrap()
    };
    unsafe {
        let mut y = uninitialized_vec(batch_size * size_per_batch_y);
        let f;
        #[cfg(feature = "mkl")] { f = fast_col_x_filter_kernel; }
        #[cfg(not(feature = "mkl"))] { f = slow_col_x_filter_kernel; }
        f(cols.as_slice().unwrap(), w_slice, y.as_mut_slice(), xch, ych, yh, yw, kh, kw, batch_size);
        NdArray::from_shape_vec(ndarray::IxDyn(&[batch_size, ych, yh, yw]), y).unwrap()
    }
}

fn conv2d_filter_grad_impl<F: Float>(
    cols: &NdArrayView<F>,
    gy: &NdArrayView<F>,
    w: &NdArrayView<F>,
) -> NdArray<F> {

    let k_shape = w.shape();
    let cols_shape = cols.shape();
    let gy_shape = gy.shape();

    let size_per_batch_g = { gy_shape[1] * gy_shape[2] * gy_shape[3] };
    let size_per_batch_c =
        { cols_shape[1] * cols_shape[2] * cols_shape[3] * cols_shape[4] * cols_shape[5] };
    let (xch, kh, kw) = (k_shape[1], k_shape[2], k_shape[3]);
    let (batch_size, ych, yh, yw) = (gy_shape[0], gy_shape[1], gy_shape[2], gy_shape[3]);

    let cols = cols.as_ptr();
    let copied_gy = ndarray_ext::copy_if_dirty(gy);
    let gy = copied_gy.map(|inner| inner.as_ptr()).unwrap_or(gy.as_ptr());

    unsafe {
        let mut gw = uninitialized_vec::<F>(ych * xch * kh * kw);
        let gw_head: *mut F = gw.as_mut_ptr();

        #[cfg(feature = "mkl")] {
            let m = ych as MklInt;
            let n = (kh * kw * xch) as MklInt;
            let k = (yh * yw) as MklInt;
            macro_rules! kernel_call_def { ($ty:ty, $f:ident) => {
                if crate::same_type::<$ty, F>() {
                    for i in 0..batch_size {
                        $f(CBLAS_ROW_MAJOR, CblasNoTrans, CblasTrans, m, n, k, 1.,
                           gy.offset((i * size_per_batch_g) as isize) as *const $ty, k,
                        cols.offset((i * size_per_batch_c) as isize) as *const $ty, k,
                        if i == 0 { 0. } else { 1. },
                        gw_head as *mut $ty, n);
                    }
                }
            }}
            kernel_call_def!(f32, cblas_sgemm);
            kernel_call_def!(f64, cblas_dgemm);
        }
        #[cfg(not(feature = "mkl"))] {
            let (m, n, k) = (ych, kh * kw * xch, yh * yw);
            let (rsa, csa) = (k, 1);
            let (rsb, csb) = (1, k);
            let (rsc, csc) = (n, 1);
            macro_rules! kernel_call_def { ($ty:ty, $f:ident) => {
                if crate::same_type::<$ty, F>() {
                    for i in 0..batch_size {
                        matrixmultiply::$f(
                            m, k, n,
                            1.,  // alpha
                            gy.offset((i * size_per_batch_g) as isize) as *const $ty, // a
                            rsa as isize, csa as isize,
                            cols.offset((i * size_per_batch_c) as isize) as *const $ty, // b
                            rsb as isize, csb as isize,
                            if i == 0 { 0. } else { 1. }, // beta
                            gw_head as *mut $ty,  // c
                            rsc as isize, csc as isize,
                        );
                    }
                }
            }}
            kernel_call_def!(f32, sgemm);
            kernel_call_def!(f64, dgemm);
        }

        NdArray::from_shape_vec_unchecked(k_shape, gw)
    }
}

impl<T: Float> crate::op::Op<T> for Conv2D {
    fn name(&self) -> &str {
        "Conv2D"
    }

    #[allow(unused_mut)]
    fn compute<'v>(
        &self,
        ctx: crate::runtime::OpComputeContext<'v, T>,
    ) -> crate::op::ComputeResults<'v, T> {
        // Grab inputs
        let xs = ctx.grab_inputs();
        let x = &xs[0];
        let w = &xs[1];
        let (y, cols) = conv2d_impl(x, w, self.pad, self.pad, self.stride, self.stride, self.dilation, self.dilation);
        vec![
            Ok(crate::ArrRepr::Owned(y)),
            Ok(crate::ArrRepr::Owned(cols)),
        ]
    }

    fn grad(&self, gy: &Tensor<T>, xs: &[&Tensor<T>], y: &Tensor<T>) -> Vec<Option<Tensor<T>>> {
        let x = xs[0];
        let w = xs[1];

        let gx = Tensor::builder().set_inputs(vec![gy, w]).build(
            super::conv2d_transpose::Conv2DTranspose {
                pad: self.pad,
                stride: self.stride,
                dilation: self.dilation,
            },
        );

        let cols = &crate::ops::nth_tensor(y, 1);
        let gw = Tensor::builder()
            .set_inputs(vec![cols, gy, w])
            .set_backprop_inputs(vec![x.clone(), gy.clone()])
            .build(Conv2DFilterGrad {
                pad: self.pad,
                stride: self.stride,
                dilation: self.dilation,
            });

        vec![Some(gx), Some(gw)]
    }
}

impl<T: Float> crate::op::Op<T> for Conv2DWithCols {
    fn name(&self) -> &str {
        "Conv2DWithCols"
    }

    #[allow(unused_mut)]
    fn compute<'v>(
        &self,
        ctx: crate::runtime::OpComputeContext<'v, T>,
    ) -> crate::op::ComputeResults<'v, T> {
        // Grab inputs
        let xs = ctx.grab_inputs();
        let cols = &xs[0];
        let w = &xs[1];
        let y = conv2d_with_cols_impl(cols, w);
        vec![Ok(crate::ArrRepr::Owned(y))]
    }

    fn grad(&self, gy: &Tensor<T>, xs: &[&Tensor<T>], y: &Tensor<T>) -> Vec<Option<Tensor<T>>> {
        let cols = xs[0];
        let w = xs[1];

        let gx = Tensor::builder().set_inputs(vec![gy, w]).build(
            super::conv2d_transpose::Conv2DTranspose {
                pad: self.pad,
                stride: self.stride,
                dilation: self.dilation,
            },
        );

        let gw = Tensor::builder()
            .set_inputs(vec![cols, gy, w])
            .set_backprop_inputs(vec![
                y.inputs_on_backprop.as_ref().unwrap()[0].clone(),
                gy.clone(),
            ])
            .build(Conv2DFilterGrad {
                pad: self.pad,
                stride: self.stride,
                dilation: self.dilation,
            });

        vec![Some(gx), Some(gw)]
    }
}

impl<T: Float> crate::op::Op<T> for Conv2DFilterGrad {
    fn name(&self) -> &str {
        "Conv2DFilterGrad"
    }

    fn compute<'v>(
        &self,
        ctx: crate::runtime::OpComputeContext<'v, T>,
    ) -> crate::op::ComputeResults<'v, T> {
        let xs = ctx.grab_inputs();
        let cols = &xs[0]; // must be columns
        let gy = &xs[1];
        let w = &xs[2];
        let gw = conv2d_filter_grad_impl(cols, gy, w);
        vec![Ok(crate::ArrRepr::Owned(gw))]
    }

    fn grad(&self, ggw: &Tensor<T>, xs: &[&Tensor<T>], y: &Tensor<T>) -> Vec<Option<Tensor<T>>> {
        let cols = xs[0];
        let gy = xs[1]; // For example, gradient of output of Conv2D.

        // grad grad
        let gx = Tensor::builder().set_inputs(vec![gy, ggw]).build(
            super::conv2d_transpose::Conv2DTranspose {
                pad: self.pad,
                stride: self.stride,
                dilation: self.dilation,
            },
        );

        let ggy = Tensor::builder()
            .set_inputs(vec![cols, ggw])
            .set_backprop_inputs(vec![
                y.inputs_on_backprop.as_ref().unwrap()[0].clone(),
                ggw.clone(),
            ])
            .build(Conv2DWithCols {
                pad: self.pad,
                stride: self.stride,
                dilation: self.dilation,
            });

        vec![Some(gx), Some(ggy), None]
    }
}

#[test]
fn test_conv2d() {
    use crate::op::Op;
    let op = Conv2D {
        pad: 0,
        stride: 1,
        dilation: 1,
    };

    let x = ndarray::Array1::range(0., 2. * 2. * 3. * 3., 1.)
        .into_shape((2, 2, 3, 3))
        .unwrap()
        .into_dyn();

    let w = crate::ndarray_ext::ones(&[
        /*out_ch=*/ 2, /*in_ch=*/ 2, /*row=*/ 2, /*col=*/ 2,
    ]);

    let y = op.compute(crate::runtime::OpComputeContext::new(
        vec![crate::zeros(&[1])], // dummy
        vec![x.view(), w.view()],
    ));

    assert_eq!(
        y[0].as_ref().unwrap().to_owned().as_slice().unwrap(),
        &[52., 60., 76., 84., 52., 60., 76., 84., 196., 204., 220., 228., 196., 204., 220., 228.,]
    );
}
