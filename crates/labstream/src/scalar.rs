//! The sample types that a stream can carry.

use labstream_wire::{Format, Value};

mod sealed {
    pub trait Sealed {}
}

/// A numeric type that an inlet can read and an outlet can write.
///
/// The trait is sealed. Only the seven formats of the LSL protocol implement it.
///
/// A pull converts. If an outlet writes `i16` and the program reads `f32`, the
/// value converts on arrival. liblsl converts in the same place
/// (`include/lsl_cpp.h:1053-1077`), so a program that moves from liblsl keeps its
/// behavior.
pub trait Scalar: Copy + Default + sealed::Sealed {
    /// The wire format of this type.
    const FORMAT: Format;
    /// Convert one value from the wire. A string value gives the default.
    fn from_value(v: &Value) -> Self;
    /// Convert one value for the wire.
    fn into_value(self) -> Value;
}

macro_rules! scalar {
    ($t:ty, $fmt:ident, $var:ident) => {
        impl sealed::Sealed for $t {}
        impl Scalar for $t {
            const FORMAT: Format = Format::$fmt;
            fn from_value(v: &Value) -> Self {
                match *v {
                    Value::F32(x) => x as $t,
                    Value::F64(x) => x as $t,
                    Value::I8(x) => x as $t,
                    Value::I16(x) => x as $t,
                    Value::I32(x) => x as $t,
                    Value::I64(x) => x as $t,
                    Value::Str(_) => <$t>::default(),
                }
            }
            fn into_value(self) -> Value {
                Value::$var(self)
            }
        }
    };
}

scalar!(f32, Float32, F32);
scalar!(f64, Double64, F64);
scalar!(i8, Int8, I8);
scalar!(i16, Int16, I16);
scalar!(i32, Int32, I32);
scalar!(i64, Int64, I64);

/// Read one value as a number, whatever format it arrived in.
///
/// A string value gives `None`. Use this for a stream whose format the program
/// learns at run time.
pub fn as_f64(v: &Value) -> Option<f64> {
    Some(match *v {
        Value::F32(x) => x as f64,
        Value::F64(x) => x,
        Value::I8(x) => x as f64,
        Value::I16(x) => x as f64,
        Value::I32(x) => x as f64,
        Value::I64(x) => x as f64,
        Value::Str(_) => return None,
    })
}

/// Read one value as text.
///
/// A string value gives its text. LSL does not require valid UTF-8, so a byte
/// that is not valid becomes the replacement character. A numeric value gives its
/// number as text, because many marker streams carry integer trigger codes.
pub fn as_text(v: &Value) -> String {
    match v {
        Value::Str(b) => String::from_utf8_lossy(b).into_owned(),
        other => as_f64(other).map(|n| n.to_string()).unwrap_or_default(),
    }
}
