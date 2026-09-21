const BASIS_POINTS: f64 = 10_000.0;

// Constant-product average execution loss against pre-trade spot, excluding fees and rounding.
// Floating point is only for this display metric; swap amounts remain integer calculations.
pub(crate) fn exact_input(input_reserve: u64, output_reserve: u64, input: u64) -> Option<f64> {
    if input_reserve == 0 || output_reserve == 0 || input == 0 {
        return None;
    }
    Some(BASIS_POINTS * input as f64 / (input_reserve as f64 + input as f64))
}

pub(crate) fn exact_output(output_reserve: u64, output: u64) -> Option<f64> {
    if output == 0 || output >= output_reserve {
        return None;
    }
    Some(BASIS_POINTS * output as f64 / output_reserve as f64)
}

pub(crate) fn combine(first: Option<f64>, second: Option<f64>) -> Option<f64> {
    let (first, second) = (first?, second?);
    Some(first + second - first * second / BASIS_POINTS)
}

#[cfg(test)]
mod tests;
