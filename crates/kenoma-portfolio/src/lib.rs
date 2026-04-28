//! Multi-asset portfolio accounting for research backtests.

use kenoma_types::{
    AssetClass, Currency, Fill, InstrumentId, InstrumentSpec, OrderSide, Position, Price, Quantity,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PortfolioError {
    #[error("missing FX rate {0}->{1}")]
    MissingFxRate(Currency, Currency),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Portfolio {
    pub base_currency: Currency,
    #[serde(default)]
    pub cash: BTreeMap<Currency, f64>,
    #[serde(default)]
    pub positions: BTreeMap<InstrumentId, Position>,
    #[serde(default)]
    pub marks: BTreeMap<InstrumentId, Price>,
    #[serde(default)]
    pub fx_rates: BTreeMap<(Currency, Currency), f64>,
    #[serde(default)]
    pub total_fees: f64,
}

impl Portfolio {
    pub fn new(base_currency: impl Into<Currency>, initial_capital: f64) -> Self {
        let base_currency = base_currency.into();
        let mut cash = BTreeMap::new();
        cash.insert(base_currency.clone(), initial_capital);
        Self {
            base_currency,
            cash,
            positions: BTreeMap::new(),
            marks: BTreeMap::new(),
            fx_rates: BTreeMap::new(),
            total_fees: 0.0,
        }
    }

    pub fn set_fx_rate(&mut self, from: impl Into<Currency>, to: impl Into<Currency>, rate: f64) {
        let from = from.into();
        let to = to.into();
        self.fx_rates.insert((from.clone(), to.clone()), rate);
        if rate != 0.0 {
            self.fx_rates.insert((to, from), 1.0 / rate);
        }
    }

    pub fn fx(&self, from: &str, to: &str) -> Result<f64, PortfolioError> {
        if from == to {
            return Ok(1.0);
        }
        self.fx_rates
            .get(&(from.to_string(), to.to_string()))
            .copied()
            .ok_or_else(|| PortfolioError::MissingFxRate(from.to_string(), to.to_string()))
    }

    pub fn apply_fill(&mut self, fill: &Fill, spec: &InstrumentSpec) -> Result<(), PortfolioError> {
        let fee_base = fill.fee * self.fx(&spec.quote_currency, &self.base_currency)?;
        self.total_fees += fee_base;
        *self.cash.entry(self.base_currency.clone()).or_default() -= fee_base;

        let multiplier = spec.multiplier;
        let signed_qty = fill.qty * fill.side.sign();
        let notional_quote = fill.price * fill.qty.abs() * multiplier;
        let quote_to_base = self.fx(&spec.quote_currency, &self.base_currency)?;

        let position = self
            .positions
            .entry(fill.instrument_id)
            .or_insert_with(|| Position::flat(fill.instrument_id));

        let old_realized = position.realized_pnl;
        update_position(position, signed_qty, fill.price, multiplier);
        position.fees += fill.fee;

        match spec.asset_class {
            AssetClass::Future | AssetClass::CryptoPerpetual => {
                let realized_delta = position.realized_pnl - old_realized;
                let realized_base = realized_delta * quote_to_base;
                *self.cash.entry(self.base_currency.clone()).or_default() += realized_base;
            }
            AssetClass::Equity | AssetClass::CryptoSpot | AssetClass::Fx | AssetClass::Option => {
                let cash_flow_quote = match fill.side {
                    OrderSide::Buy => -notional_quote,
                    OrderSide::Sell => notional_quote,
                };
                *self.cash.entry(self.base_currency.clone()).or_default() +=
                    cash_flow_quote * quote_to_base;
            }
        }

        self.marks.insert(fill.instrument_id, fill.price);
        Ok(())
    }

    pub fn mark(&mut self, instrument_id: InstrumentId, price: Price) {
        self.marks.insert(instrument_id, price);
    }

    pub fn accrue_funding(
        &mut self,
        instrument_id: InstrumentId,
        spec: &InstrumentSpec,
        rate: f64,
    ) -> Result<f64, PortfolioError> {
        let Some(position) = self.positions.get(&instrument_id) else {
            return Ok(0.0);
        };
        let mark = self
            .marks
            .get(&instrument_id)
            .copied()
            .unwrap_or(position.avg_price);
        let payment_quote = position.qty * mark * spec.multiplier * rate;
        let payment_base = payment_quote * self.fx(&spec.quote_currency, &self.base_currency)?;
        *self.cash.entry(self.base_currency.clone()).or_default() -= payment_base;
        Ok(payment_base)
    }

    pub fn accrue_borrow(
        &mut self,
        instrument_id: InstrumentId,
        spec: &InstrumentSpec,
        annualized_rate: f64,
        days: f64,
    ) -> Result<f64, PortfolioError> {
        let Some(position) = self.positions.get(&instrument_id) else {
            return Ok(0.0);
        };
        if position.qty >= 0.0 {
            return Ok(0.0);
        }
        let mark = self
            .marks
            .get(&instrument_id)
            .copied()
            .unwrap_or(position.avg_price);
        let borrow_quote =
            position.qty.abs() * mark * spec.multiplier * annualized_rate * days / 365.0;
        let borrow_base = borrow_quote * self.fx(&spec.quote_currency, &self.base_currency)?;
        *self.cash.entry(self.base_currency.clone()).or_default() -= borrow_base;
        Ok(borrow_base)
    }

    pub fn equity(
        &self,
        instruments: &BTreeMap<InstrumentId, InstrumentSpec>,
    ) -> Result<f64, PortfolioError> {
        let mut total = 0.0;
        for (currency, cash) in &self.cash {
            total += cash * self.fx(currency, &self.base_currency)?;
        }
        for (instrument_id, position) in &self.positions {
            if position.qty == 0.0 {
                continue;
            }
            let Some(spec) = instruments.get(instrument_id) else {
                continue;
            };
            let mark = self
                .marks
                .get(instrument_id)
                .copied()
                .unwrap_or(position.avg_price);
            let value_quote = position_value(position, spec, mark);
            total += value_quote * self.fx(&spec.quote_currency, &self.base_currency)?;
        }
        Ok(total)
    }

    pub fn positions_vec(&self) -> Vec<Position> {
        self.positions.values().cloned().collect()
    }
}

fn update_position(
    position: &mut Position,
    signed_qty_delta: Quantity,
    price: Price,
    multiplier: f64,
) {
    let old_qty = position.qty;
    if old_qty == 0.0 || old_qty.signum() == signed_qty_delta.signum() {
        let new_qty = old_qty + signed_qty_delta;
        position.avg_price = if new_qty == 0.0 {
            0.0
        } else {
            (old_qty.abs() * position.avg_price + signed_qty_delta.abs() * price) / new_qty.abs()
        };
        position.qty = new_qty;
        return;
    }

    let close_qty = old_qty.abs().min(signed_qty_delta.abs());
    position.realized_pnl +=
        close_qty * (price - position.avg_price) * old_qty.signum() * multiplier;
    let new_qty = old_qty + signed_qty_delta;
    position.qty = new_qty;
    position.avg_price = if new_qty == 0.0 {
        0.0
    } else if old_qty.signum() != new_qty.signum() {
        price
    } else {
        position.avg_price
    };
}

fn position_value(position: &Position, spec: &InstrumentSpec, mark: Price) -> f64 {
    match spec.asset_class {
        AssetClass::Future | AssetClass::CryptoPerpetual => {
            (mark - position.avg_price) * position.qty * spec.multiplier
        }
        AssetClass::Equity | AssetClass::CryptoSpot | AssetClass::Fx | AssetClass::Option => {
            mark * position.qty * spec.multiplier
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kenoma_types::{FeeSpec, OrderSide};

    fn future_spec() -> InstrumentSpec {
        InstrumentSpec {
            id: 1,
            symbol: "MES".to_string(),
            asset_class: AssetClass::Future,
            tick_size: 0.25,
            lot_size: 1.0,
            multiplier: 5.0,
            quote_currency: "USD".to_string(),
            base_currency: None,
            session_calendar: None,
            fees: FeeSpec::default(),
            funding: None,
            borrow: None,
            contract: None,
            option: None,
            metadata: Default::default(),
        }
    }

    fn equity_spec() -> InstrumentSpec {
        InstrumentSpec {
            id: 2,
            symbol: "ABC".to_string(),
            asset_class: AssetClass::Equity,
            tick_size: 0.01,
            lot_size: 1.0,
            multiplier: 1.0,
            quote_currency: "EUR".to_string(),
            base_currency: None,
            session_calendar: None,
            fees: FeeSpec::default(),
            funding: None,
            borrow: None,
            contract: None,
            option: None,
            metadata: Default::default(),
        }
    }

    #[test]
    fn futures_multiplier_drives_unrealized_pnl() {
        let spec = future_spec();
        let mut instruments = BTreeMap::new();
        instruments.insert(spec.id, spec.clone());
        let mut portfolio = Portfolio::new("USD", 10_000.0);
        portfolio
            .apply_fill(
                &Fill {
                    order_id: 1,
                    instrument_id: 1,
                    ts: 1,
                    side: OrderSide::Buy,
                    price: 4000.0,
                    qty: 2.0,
                    fee: 0.0,
                    liquidity: None,
                },
                &spec,
            )
            .unwrap();
        portfolio.mark(1, 4001.0);
        assert_eq!(portfolio.equity(&instruments).unwrap(), 10_010.0);
    }

    #[test]
    fn fx_conversion_marks_foreign_equity() {
        let spec = equity_spec();
        let mut instruments = BTreeMap::new();
        instruments.insert(spec.id, spec.clone());
        let mut portfolio = Portfolio::new("USD", 1_000.0);
        portfolio.set_fx_rate("EUR", "USD", 1.2);
        portfolio
            .apply_fill(
                &Fill {
                    order_id: 1,
                    instrument_id: 2,
                    ts: 1,
                    side: OrderSide::Buy,
                    price: 10.0,
                    qty: 10.0,
                    fee: 0.0,
                    liquidity: None,
                },
                &spec,
            )
            .unwrap();
        portfolio.mark(2, 11.0);
        assert!((portfolio.equity(&instruments).unwrap() - 1_012.0).abs() < 1e-9);
    }

    #[test]
    fn total_fees_are_reported_in_base_currency() {
        let spec = equity_spec();
        let mut portfolio = Portfolio::new("USD", 1_000.0);
        portfolio.set_fx_rate("EUR", "USD", 1.2);
        portfolio
            .apply_fill(
                &Fill {
                    order_id: 1,
                    instrument_id: 2,
                    ts: 1,
                    side: OrderSide::Buy,
                    price: 10.0,
                    qty: 1.0,
                    fee: 2.0,
                    liquidity: None,
                },
                &spec,
            )
            .unwrap();
        assert!((portfolio.total_fees - 2.4).abs() < 1e-9);
    }

    #[test]
    fn short_borrow_debits_cash() {
        let spec = InstrumentSpec {
            quote_currency: "USD".to_string(),
            ..equity_spec()
        };
        let mut portfolio = Portfolio::new("USD", 1_000.0);
        portfolio
            .apply_fill(
                &Fill {
                    order_id: 1,
                    instrument_id: 2,
                    ts: 1,
                    side: OrderSide::Sell,
                    price: 20.0,
                    qty: 10.0,
                    fee: 0.0,
                    liquidity: None,
                },
                &spec,
            )
            .unwrap();
        let charged = portfolio.accrue_borrow(2, &spec, 0.365, 1.0).unwrap();
        assert!((charged - 0.2).abs() < 1e-9);
    }
}
