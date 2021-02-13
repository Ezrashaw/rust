pub mod array;

#[macro_use]
pub mod biteq;

pub trait DefaultStrategy {
    type Strategy: proptest::strategy::Strategy<Value = Self>;
    fn default_strategy() -> Self::Strategy;
}

macro_rules! impl_num {
    { $type:tt } => {
        impl DefaultStrategy for $type {
            type Strategy = proptest::num::$type::Any;
            fn default_strategy() -> Self::Strategy {
                proptest::num::$type::ANY
            }
        }
    }
}

impl_num! { i8 }
impl_num! { i16 }
impl_num! { i32 }
impl_num! { i64 }
impl_num! { i128 }
impl_num! { isize }
impl_num! { u8 }
impl_num! { u16 }
impl_num! { u32 }
impl_num! { u64 }
impl_num! { u128 }
impl_num! { usize }
impl_num! { f32 }
impl_num! { f64 }

impl<T: core::fmt::Debug + DefaultStrategy, const LANES: usize> DefaultStrategy for [T; LANES] {
    type Strategy = crate::array::UniformArrayStrategy<T::Strategy, Self>;
    fn default_strategy() -> Self::Strategy {
        Self::Strategy::new(T::default_strategy())
    }
}

pub fn test_1<A: core::fmt::Debug + DefaultStrategy>(
    f: &dyn Fn(A) -> proptest::test_runner::TestCaseResult,
) {
    let mut runner = proptest::test_runner::TestRunner::default();
    runner.run(&A::default_strategy(), f).unwrap();
}

pub fn test_2<A: core::fmt::Debug + DefaultStrategy, B: core::fmt::Debug + DefaultStrategy>(
    f: &dyn Fn(A, B) -> proptest::test_runner::TestCaseResult,
) {
    let mut runner = proptest::test_runner::TestRunner::default();
    runner
        .run(&(A::default_strategy(), B::default_strategy()), |(a, b)| {
            f(a, b)
        })
        .unwrap();
}

#[inline(never)]
pub fn test_unary_elementwise<Scalar, ScalarResult, Vector, VectorResult, const LANES: usize>(
    fv: &dyn Fn(Vector) -> VectorResult,
    fs: &dyn Fn(Scalar) -> ScalarResult,
    check: &dyn Fn([Scalar; LANES]) -> bool,
) where
    Scalar: Copy + Default + core::fmt::Debug + DefaultStrategy,
    ScalarResult: Copy + Default + biteq::BitEq + core::fmt::Debug + DefaultStrategy,
    Vector: Into<[Scalar; LANES]> + From<[Scalar; LANES]> + Copy,
    VectorResult: Into<[ScalarResult; LANES]> + From<[ScalarResult; LANES]> + Copy,
{
    test_1(&|x: [Scalar; LANES]| {
        proptest::prop_assume!(check(x));
        let result_1: [ScalarResult; LANES] = fv(x.into()).into();
        let result_2: [ScalarResult; LANES] = {
            let mut result = [ScalarResult::default(); LANES];
            for (i, o) in x.iter().zip(result.iter_mut()) {
                *o = fs(*i);
            }
            result
        };
        crate::prop_assert_biteq!(result_1, result_2);
        Ok(())
    });
}

#[inline(never)]
pub fn test_binary_elementwise<
    Scalar1,
    Scalar2,
    ScalarResult,
    Vector1,
    Vector2,
    VectorResult,
    const LANES: usize,
>(
    fv: &dyn Fn(Vector1, Vector2) -> VectorResult,
    fs: &dyn Fn(Scalar1, Scalar2) -> ScalarResult,
    check: &dyn Fn([Scalar1; LANES], [Scalar2; LANES]) -> bool,
) where
    Scalar1: Copy + Default + core::fmt::Debug + DefaultStrategy,
    Scalar2: Copy + Default + core::fmt::Debug + DefaultStrategy,
    ScalarResult: Copy + Default + biteq::BitEq + core::fmt::Debug + DefaultStrategy,
    Vector1: Into<[Scalar1; LANES]> + From<[Scalar1; LANES]> + Copy,
    Vector2: Into<[Scalar2; LANES]> + From<[Scalar2; LANES]> + Copy,
    VectorResult: Into<[ScalarResult; LANES]> + From<[ScalarResult; LANES]> + Copy,
{
    test_2(&|x: [Scalar1; LANES], y: [Scalar2; LANES]| {
        proptest::prop_assume!(check(x, y));
        let result_1: [ScalarResult; LANES] = fv(x.into(), y.into()).into();
        let result_2: [ScalarResult; LANES] = {
            let mut result = [ScalarResult::default(); LANES];
            for ((i1, i2), o) in x.iter().zip(y.iter()).zip(result.iter_mut()) {
                *o = fs(*i1, *i2);
            }
            result
        };
        crate::prop_assert_biteq!(result_1, result_2);
        Ok(())
    });
}

#[inline(never)]
pub fn test_binary_scalar_rhs_elementwise<
    Scalar1,
    Scalar2,
    ScalarResult,
    Vector,
    VectorResult,
    const LANES: usize,
>(
    fv: &dyn Fn(Vector, Scalar2) -> VectorResult,
    fs: &dyn Fn(Scalar1, Scalar2) -> ScalarResult,
    check: &dyn Fn([Scalar1; LANES], Scalar2) -> bool,
) where
    Scalar1: Copy + Default + core::fmt::Debug + DefaultStrategy,
    Scalar2: Copy + Default + core::fmt::Debug + DefaultStrategy,
    ScalarResult: Copy + Default + biteq::BitEq + core::fmt::Debug + DefaultStrategy,
    Vector: Into<[Scalar1; LANES]> + From<[Scalar1; LANES]> + Copy,
    VectorResult: Into<[ScalarResult; LANES]> + From<[ScalarResult; LANES]> + Copy,
{
    test_2(&|x: [Scalar1; LANES], y: Scalar2| {
        proptest::prop_assume!(check(x, y));
        let result_1: [ScalarResult; LANES] = fv(x.into(), y).into();
        let result_2: [ScalarResult; LANES] = {
            let mut result = [ScalarResult::default(); LANES];
            for (i, o) in x.iter().zip(result.iter_mut()) {
                *o = fs(*i, y);
            }
            result
        };
        crate::prop_assert_biteq!(result_1, result_2);
        Ok(())
    });
}

#[inline(never)]
pub fn test_binary_scalar_lhs_elementwise<
    Scalar1,
    Scalar2,
    ScalarResult,
    Vector,
    VectorResult,
    const LANES: usize,
>(
    fv: &dyn Fn(Scalar1, Vector) -> VectorResult,
    fs: &dyn Fn(Scalar1, Scalar2) -> ScalarResult,
    check: &dyn Fn(Scalar1, [Scalar2; LANES]) -> bool,
) where
    Scalar1: Copy + Default + core::fmt::Debug + DefaultStrategy,
    Scalar2: Copy + Default + core::fmt::Debug + DefaultStrategy,
    ScalarResult: Copy + Default + biteq::BitEq + core::fmt::Debug + DefaultStrategy,
    Vector: Into<[Scalar2; LANES]> + From<[Scalar2; LANES]> + Copy,
    VectorResult: Into<[ScalarResult; LANES]> + From<[ScalarResult; LANES]> + Copy,
{
    test_2(&|x: Scalar1, y: [Scalar2; LANES]| {
        proptest::prop_assume!(check(x, y));
        let result_1: [ScalarResult; LANES] = fv(x, y.into()).into();
        let result_2: [ScalarResult; LANES] = {
            let mut result = [ScalarResult::default(); LANES];
            for (i, o) in y.iter().zip(result.iter_mut()) {
                *o = fs(x, *i);
            }
            result
        };
        crate::prop_assert_biteq!(result_1, result_2);
        Ok(())
    });
}

#[macro_export]
macro_rules! test_lanes {
    {
        $(fn $test:ident<const $lanes:ident: usize>() $body:tt)*
    } => {
        $(
            mod $test {
                use super::*;

                fn implementation<const $lanes: usize>() $body

                #[test]
                #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
                fn lanes_1() {
                    implementation::<1>();
                }

                #[test]
                #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
                fn lanes_2() {
                    implementation::<2>();
                }

                #[test]
                #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
                fn lanes_4() {
                    implementation::<4>();
                }

                #[test]
                #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
                fn lanes_8() {
                    implementation::<8>();
                }

                #[test]
                #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
                fn lanes_16() {
                    implementation::<16>();
                }

                #[test]
                #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
                fn lanes_32() {
                    implementation::<32>();
                }

                #[test]
                #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
                fn lanes_64() {
                    implementation::<64>();
                }
            }
        )*
    }
}
