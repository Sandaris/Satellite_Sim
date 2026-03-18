#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Celsius(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Watts(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct RadPerSec(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Microseconds(pub u64);

pub fn is_thermal_safe(temp: Celsius) -> bool {
    temp.0 < 70.0
}
