//! Options pricing models, Greeks, and implied volatility solving.

use kenoma_types::OptionKind;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum OptionError {
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("implied volatility did not converge")]
    NoConvergence,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OptionInputs {
    pub kind: OptionKind,
    pub spot: f64,
    pub strike: f64,
    pub rate: f64,
    #[serde(default)]
    pub dividend_yield: f64,
    pub volatility: f64,
    pub time_to_expiry_years: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Black76Inputs {
    pub kind: OptionKind,
    pub forward: f64,
    pub strike: f64,
    pub rate: f64,
    pub volatility: f64,
    pub time_to_expiry_years: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Greeks {
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub rho: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exercise {
    European,
    American,
}

pub fn black_scholes_merton_price(input: OptionInputs) -> Result<f64, OptionError> {
    validate_positive(input.spot, "spot")?;
    validate_positive(input.strike, "strike")?;
    validate_nonnegative(input.volatility, "volatility")?;
    validate_nonnegative(input.time_to_expiry_years, "time_to_expiry_years")?;
    if input.time_to_expiry_years == 0.0 || input.volatility == 0.0 {
        return Ok(discounted_intrinsic(input));
    }
    let (d1, d2) = bsm_d1_d2(input);
    let s_disc = input.spot * (-input.dividend_yield * input.time_to_expiry_years).exp();
    let k_disc = input.strike * (-input.rate * input.time_to_expiry_years).exp();
    Ok(match input.kind {
        OptionKind::Call => s_disc * norm_cdf(d1) - k_disc * norm_cdf(d2),
        OptionKind::Put => k_disc * norm_cdf(-d2) - s_disc * norm_cdf(-d1),
    })
}

pub fn black_scholes_merton_greeks(input: OptionInputs) -> Result<Greeks, OptionError> {
    validate_positive(input.spot, "spot")?;
    validate_positive(input.strike, "strike")?;
    validate_positive(input.volatility, "volatility")?;
    validate_positive(input.time_to_expiry_years, "time_to_expiry_years")?;

    let (d1, d2) = bsm_d1_d2(input);
    let t = input.time_to_expiry_years;
    let sqrt_t = t.sqrt();
    let q_disc = (-input.dividend_yield * t).exp();
    let r_disc = (-input.rate * t).exp();
    let pdf = norm_pdf(d1);
    let gamma = q_disc * pdf / (input.spot * input.volatility * sqrt_t);
    let vega = input.spot * q_disc * pdf * sqrt_t;
    let theta_common = -input.spot * q_disc * pdf * input.volatility / (2.0 * sqrt_t);
    Ok(match input.kind {
        OptionKind::Call => Greeks {
            delta: q_disc * norm_cdf(d1),
            gamma,
            theta: theta_common - input.rate * input.strike * r_disc * norm_cdf(d2)
                + input.dividend_yield * input.spot * q_disc * norm_cdf(d1),
            vega,
            rho: input.strike * t * r_disc * norm_cdf(d2),
        },
        OptionKind::Put => Greeks {
            delta: q_disc * (norm_cdf(d1) - 1.0),
            gamma,
            theta: theta_common + input.rate * input.strike * r_disc * norm_cdf(-d2)
                - input.dividend_yield * input.spot * q_disc * norm_cdf(-d1),
            vega,
            rho: -input.strike * t * r_disc * norm_cdf(-d2),
        },
    })
}

pub fn black76_price(input: Black76Inputs) -> Result<f64, OptionError> {
    validate_positive(input.forward, "forward")?;
    validate_positive(input.strike, "strike")?;
    validate_nonnegative(input.volatility, "volatility")?;
    validate_nonnegative(input.time_to_expiry_years, "time_to_expiry_years")?;
    if input.time_to_expiry_years == 0.0 || input.volatility == 0.0 {
        return Ok(match input.kind {
            OptionKind::Call => (input.forward - input.strike).max(0.0),
            OptionKind::Put => (input.strike - input.forward).max(0.0),
        } * (-input.rate * input.time_to_expiry_years).exp());
    }
    let t = input.time_to_expiry_years;
    let sigma_sqrt_t = input.volatility * t.sqrt();
    let d1 =
        ((input.forward / input.strike).ln() + 0.5 * input.volatility.powi(2) * t) / sigma_sqrt_t;
    let d2 = d1 - sigma_sqrt_t;
    let discount = (-input.rate * t).exp();
    Ok(match input.kind {
        OptionKind::Call => discount * (input.forward * norm_cdf(d1) - input.strike * norm_cdf(d2)),
        OptionKind::Put => {
            discount * (input.strike * norm_cdf(-d2) - input.forward * norm_cdf(-d1))
        }
    })
}

pub fn crr_binomial_price(
    input: OptionInputs,
    steps: usize,
    exercise: Exercise,
) -> Result<f64, OptionError> {
    validate_positive(input.spot, "spot")?;
    validate_positive(input.strike, "strike")?;
    validate_positive(input.volatility, "volatility")?;
    validate_positive(input.time_to_expiry_years, "time_to_expiry_years")?;
    if steps == 0 {
        return Err(OptionError::InvalidInput("steps"));
    }

    let dt = input.time_to_expiry_years / steps as f64;
    let up = (input.volatility * dt.sqrt()).exp();
    let down = 1.0 / up;
    let growth = ((input.rate - input.dividend_yield) * dt).exp();
    let p = (growth - down) / (up - down);
    if !(0.0..=1.0).contains(&p) {
        return Err(OptionError::InvalidInput("risk neutral probability"));
    }
    let discount = (-input.rate * dt).exp();

    let mut values = vec![0.0; steps + 1];
    for (i, value) in values.iter_mut().enumerate() {
        let spot = input.spot * up.powi(i as i32) * down.powi((steps - i) as i32);
        *value = intrinsic(input.kind, spot, input.strike);
    }

    for step in (0..steps).rev() {
        for i in 0..=step {
            let continuation = discount * (p * values[i + 1] + (1.0 - p) * values[i]);
            if exercise == Exercise::American {
                let spot = input.spot * up.powi(i as i32) * down.powi((step - i) as i32);
                values[i] = continuation.max(intrinsic(input.kind, spot, input.strike));
            } else {
                values[i] = continuation;
            }
        }
    }
    Ok(values[0])
}

pub fn implied_volatility_bsm(
    mut input: OptionInputs,
    market_price: f64,
    tolerance: f64,
    max_iterations: usize,
) -> Result<f64, OptionError> {
    validate_positive(market_price, "market_price")?;
    validate_nonnegative(tolerance, "tolerance")?;
    validate_bsm_market_price(input, market_price, tolerance)?;
    let mut lo = 1e-6;
    let mut hi = 5.0;
    for _ in 0..max_iterations {
        input.volatility = (lo + hi) / 2.0;
        let price = black_scholes_merton_price(input)?;
        if (price - market_price).abs() <= tolerance {
            return Ok(input.volatility);
        }
        if price > market_price {
            hi = input.volatility;
        } else {
            lo = input.volatility;
        }
    }
    Err(OptionError::NoConvergence)
}

fn validate_bsm_market_price(
    input: OptionInputs,
    market_price: f64,
    tolerance: f64,
) -> Result<(), OptionError> {
    validate_positive(input.spot, "spot")?;
    validate_positive(input.strike, "strike")?;
    validate_nonnegative(input.time_to_expiry_years, "time_to_expiry_years")?;
    let t = input.time_to_expiry_years;
    let lower = discounted_intrinsic(OptionInputs {
        volatility: 0.0,
        ..input
    });
    let upper = match input.kind {
        OptionKind::Call => input.spot * (-input.dividend_yield * t).exp(),
        OptionKind::Put => input.strike * (-input.rate * t).exp(),
    };
    if market_price + tolerance < lower || market_price - tolerance > upper {
        Err(OptionError::InvalidInput("market_price"))
    } else {
        Ok(())
    }
}

fn bsm_d1_d2(input: OptionInputs) -> (f64, f64) {
    let t = input.time_to_expiry_years;
    let sigma_sqrt_t = input.volatility * t.sqrt();
    let d1 = ((input.spot / input.strike).ln()
        + (input.rate - input.dividend_yield + 0.5 * input.volatility.powi(2)) * t)
        / sigma_sqrt_t;
    (d1, d1 - sigma_sqrt_t)
}

fn discounted_intrinsic(input: OptionInputs) -> f64 {
    let t = input.time_to_expiry_years;
    let spot_disc = input.spot * (-input.dividend_yield * t).exp();
    let strike_disc = input.strike * (-input.rate * t).exp();
    match input.kind {
        OptionKind::Call => (spot_disc - strike_disc).max(0.0),
        OptionKind::Put => (strike_disc - spot_disc).max(0.0),
    }
}

fn intrinsic(kind: OptionKind, spot: f64, strike: f64) -> f64 {
    match kind {
        OptionKind::Call => (spot - strike).max(0.0),
        OptionKind::Put => (strike - spot).max(0.0),
    }
}

fn validate_positive(value: f64, name: &'static str) -> Result<(), OptionError> {
    if value > 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(OptionError::InvalidInput(name))
    }
}

fn validate_nonnegative(value: f64, name: &'static str) -> Result<(), OptionError> {
    if value >= 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(OptionError::InvalidInput(name))
    }
}

pub fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

pub fn norm_cdf(x: f64) -> f64 {
    // Abramowitz-Stegun approximation, absolute error around 7.5e-8.
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * z);
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let erf = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-z * z).exp();
    0.5 * (1.0 + sign * erf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_input() -> OptionInputs {
        OptionInputs {
            kind: OptionKind::Call,
            spot: 100.0,
            strike: 100.0,
            rate: 0.05,
            dividend_yield: 0.0,
            volatility: 0.2,
            time_to_expiry_years: 1.0,
        }
    }

    #[test]
    fn bsm_known_call_value() {
        let price = black_scholes_merton_price(call_input()).unwrap();
        assert!((price - 10.4506).abs() < 1e-3);
    }

    #[test]
    fn bsm_zero_vol_uses_discounted_forward_intrinsic() {
        let price = black_scholes_merton_price(OptionInputs {
            kind: OptionKind::Call,
            spot: 90.0,
            strike: 100.0,
            rate: 0.10,
            dividend_yield: 0.0,
            volatility: 0.0,
            time_to_expiry_years: 2.0,
        })
        .unwrap();
        let expected = 90.0 - 100.0 * (-0.20_f64).exp();
        assert!((price - expected).abs() < 1e-12);
    }

    #[test]
    fn bsm_greeks_match_known_values() {
        let greeks = black_scholes_merton_greeks(call_input()).unwrap();
        assert!((greeks.delta - 0.6368).abs() < 1e-3);
        assert!((greeks.gamma - 0.01876).abs() < 1e-4);
        assert!((greeks.vega - 37.524).abs() < 1e-2);
        assert!((greeks.rho - 53.232).abs() < 1e-2);
    }

    #[test]
    fn black76_known_atm_call() {
        let price = black76_price(Black76Inputs {
            kind: OptionKind::Call,
            forward: 100.0,
            strike: 100.0,
            rate: 0.05,
            volatility: 0.2,
            time_to_expiry_years: 1.0,
        })
        .unwrap();
        assert!((price - 7.577).abs() < 1e-3);
    }

    #[test]
    fn crr_converges_near_bsm() {
        let crr = crr_binomial_price(call_input(), 500, Exercise::European).unwrap();
        let bsm = black_scholes_merton_price(call_input()).unwrap();
        assert!((crr - bsm).abs() < 0.03);
    }

    #[test]
    fn iv_solver_round_trip() {
        let input = call_input();
        let price = black_scholes_merton_price(input).unwrap();
        let iv = implied_volatility_bsm(input, price, 1e-8, 100).unwrap();
        assert!((iv - 0.2).abs() < 1e-6);
    }

    #[test]
    fn iv_solver_rejects_no_arbitrage_violations() {
        let input = call_input();
        let err = implied_volatility_bsm(input, 101.0, 1e-8, 100).unwrap_err();
        assert_eq!(err, OptionError::InvalidInput("market_price"));
    }
}
