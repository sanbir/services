use crate::commit_reveal::SettlementSummary;
use anyhow::{Context, Result};
use model::order::{Order, OrderKind};
use num::{BigRational, ToPrimitive};
use primitive_types::{H160, U256};
use shared::{
    conversions::U256Ext, http_solver::model::TokenAmount, price_estimation::gas::GAS_PER_ORDER,
};
use solver::settlement::{
    external_prices::ExternalPrices, trade_surplus_in_native_token, verify_executed_amount,
    Interaction, Settlement, SettlementEncoder, TradeExecution,
};
use std::{
    collections::hash_map::{Entry, HashMap},
    sync::Arc,
};

/// Data describing the side effects of an on-chain interaction without the finalized call data.
#[derive(Debug, Clone)]
pub struct InteractionMetadata {
    inputs: Vec<TokenAmount>,
    outputs: Vec<TokenAmount>,
    gas_used: U256,
}

/// A type which encodes the side effects of an on-chain interaction without necesserily containing
/// the finalized call data. Is able to produce call data when it's required.
#[async_trait::async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait InteractionProposal: std::fmt::Debug + Send + Sync {
    /// Turns something like an indicative RFQ interaction into a simulatable `Interaction` by
    /// finalizing the required call data.
    async fn finalize(&self) -> Result<Arc<dyn Interaction>>;

    fn metadata(&self) -> InteractionMetadata;
}

#[derive(Debug, Clone)]
pub struct TradedOrder {
    pub order: Order,
    pub executed_amount: U256,
}

impl TradedOrder {
    /// Computes buy_token_price of the order based on whether it's a user order or a liquidity order.
    /// User orders are allowed to get surplus and therefore return the clearing price of the
    /// buy_token whereas liquidity orders must not get surplus so they return their limit price.
    fn buy_token_price(&self, clearing_prices: &HashMap<H160, U256>) -> Option<U256> {
        match self.order.metadata.is_liquidity_order {
            // liquidity orders have to be settled at their limit price
            true => clearing_prices
                .get(&self.order.data.sell_token)?
                .checked_mul(self.order.data.sell_amount)?
                .checked_div(self.order.data.buy_amount),
            false => clearing_prices.get(&self.order.data.buy_token).cloned(),
        }
    }

    /// Computes and returns the executed trade amounts given the settlements uniform clearing
    /// prices.
    fn execution(&self, clearing_prices: &HashMap<H160, U256>) -> Result<TradeExecution> {
        verify_executed_amount(&self.order, self.executed_amount)?;
        let remaining = self.order.remaining_amounts()?;

        let sell_price = clearing_prices
            .get(&self.order.data.sell_token)
            .context("no clearing price for sell token")?;
        let buy_price = self
            .buy_token_price(clearing_prices)
            .context("no clearing price for buy token")?;

        let order = &self.order.data;
        let (sell_amount, buy_amount, fee_amount) = match order.kind {
            OrderKind::Sell => {
                let sell_amount = self.executed_amount;
                let buy_amount = sell_amount
                    .checked_mul(*sell_price)
                    .and_then(|v| v.checked_ceil_div(&buy_price))
                    .context("could not compute buy amount")?;
                let fee_amount = order
                    .fee_amount
                    .checked_mul(self.executed_amount)
                    .and_then(|v| v.checked_div(order.sell_amount))
                    .context("could not compute executed fee amount")?;
                (sell_amount, buy_amount, fee_amount)
            }
            OrderKind::Buy => {
                let buy_amount = self.executed_amount;
                let sell_amount = buy_amount
                    .checked_mul(buy_price)
                    .and_then(|v| v.checked_div(*sell_price))
                    .context("could not compute sell amount")?;
                let fee_amount = order
                    .fee_amount
                    .checked_mul(self.executed_amount)
                    .and_then(|v| v.checked_div(order.buy_amount))
                    .context("could not compute executed fee amount")?;
                (sell_amount, buy_amount, fee_amount)
            }
        };

        let execution = TradeExecution {
            sell_token: order.sell_token,
            buy_token: order.buy_token,
            sell_amount,
            buy_amount,
            fee_amount,
        };

        anyhow::ensure!(
            execution.sell_amount <= remaining.sell_amount
                && execution.buy_amount >= remaining.buy_amount,
            "limit prices not respected"
        );

        Ok(execution)
    }
}

/// Contains all the information required to participate in the solver competition.
#[derive(Debug, Default, Clone)]
pub struct SettlementProposal {
    pub clearing_prices: HashMap<H160, U256>,
    pub trades: Vec<TradedOrder>,
    pub execution_plan: Vec<Arc<dyn InteractionProposal>>,
}

impl SettlementProposal {
    /// Calculates the surplus generated by this `SettlementProposal` denominated in the native
    /// token.
    pub fn surplus(&self, external_prices: &ExternalPrices) -> Result<BigRational> {
        self.trades.iter().fold(Ok(num::zero()), |acc, trade| {
            let normalized_surplus = trade_surplus_in_native_token(
                &trade.order,
                trade.executed_amount,
                external_prices,
                &self.clearing_prices,
            )
            .context("could not compute surplus for trade")?;
            Ok(acc? + normalized_surplus)
        })
    }

    /// Turns the proposal into a `SettlementEncoder` which contains finalized call data for all
    /// the interactions.
    pub async fn into_encoder(self) -> Result<SettlementEncoder> {
        let mut encoder = SettlementEncoder::new(self.clearing_prices);

        for trade in self.trades {
            let remaining_amounts = trade.order.remaining_amounts()?;

            if trade.order.metadata.is_liquidity_order {
                encoder.add_liquidity_order_trade(
                    trade.order,
                    trade.executed_amount,
                    remaining_amounts.fee_amount,
                )?;
            } else {
                encoder.add_trade(
                    trade.order,
                    trade.executed_amount,
                    remaining_amounts.fee_amount,
                )?;
            }
        }

        let futures = self
            .execution_plan
            .iter()
            .map(|interaction| interaction.finalize());
        let interactions = futures::future::try_join_all(futures).await?;
        interactions
            .into_iter()
            .flat_map(|i| i.encode())
            .for_each(|i| encoder.append_to_execution_plan(i));

        Ok(encoder)
    }

    pub async fn into_settlement(self) -> Result<Settlement> {
        Ok(Settlement {
            encoder: self.into_encoder().await?,
        })
    }

    /// Computes the `SettlementSummary` if following checks are successful:
    ///   - individual trades don't violate required properties
    ///   - enough token balances before each on-chain interaction
    ///   - enough token balances to pay out orders at the end
    ///   - solution doesn't drain settlement contract illegally
    ///   - token conservation (TODO)
    pub fn into_settlement_summary(
        &self,
        gas_price: f64,
        external_prices: &ExternalPrices,
        contract_buffer: &HashMap<H160, U256>,
    ) -> Result<SettlementSummary> {
        let mut balances = contract_buffer.clone();
        let mut gas_used = U256::zero();

        let trade_executions = self
            .trades
            .iter()
            .map(|trade| {
                trade
                    .execution(&self.clearing_prices)
                    .with_context(|| format!("could not compute trade execution: {trade:?}"))
            })
            .collect::<Result<Vec<_>>>()?;

        for (trade, execution) in self.trades.iter().zip(&trade_executions) {
            let balance = balances.entry(execution.sell_token).or_default();
            *balance = balance
                .checked_add(execution.sell_amount)
                .and_then(|b| b.checked_add(execution.fee_amount))
                .with_context(|| format!("order would overflow balance: {trade:?}"))?;
        }

        for interaction in &self.execution_plan {
            let meta = interaction.metadata();
            for input in &meta.inputs {
                match balances.entry(input.token) {
                    Entry::Occupied(mut entry) => {
                        *entry.get_mut() =
                            entry.get().checked_sub(input.amount).with_context(|| {
                                format!("interaction would underflow balance: {:?}", &meta)
                            })?;
                    }
                    _ => anyhow::bail!(format!("no balance for interaction: {:?}", input.token)),
                }
            }
            for ouput in &meta.outputs {
                let balance = balances.entry(ouput.token).or_default();
                *balance = balance
                    .checked_add(ouput.amount)
                    .with_context(|| format!("interaction would overflow balance: {:?}", &meta))?;
            }
            gas_used += meta.gas_used;
        }

        for (trade, execution) in self.trades.iter().zip(&trade_executions) {
            match balances.entry(execution.buy_token) {
                Entry::Occupied(mut entry) => {
                    *entry.get_mut() =
                        entry
                            .get()
                            .checked_sub(execution.buy_amount)
                            .with_context(|| {
                                format!("balance not big enough to pay out order: {trade:?}")
                            })?
                }
                _ => anyhow::bail!(format!("no balance to pay out order: {trade:?}")),
            }
            gas_used += GAS_PER_ORDER.into();
        }

        for (token, balance_before) in contract_buffer {
            anyhow::ensure!(
                matches!(balances.get(token), Some(balance_after) if balance_after >= balance_before),
                "solution would drain settlement contract buffers illegally"
            );
        }

        let surplus = self
            .surplus(external_prices)?
            .to_f64()
            .context("could not convert surplus to f64")?;

        let gas_reimbursement = gas_used
            .checked_mul(U256::from_f64_lossy(gas_price))
            .context("gas cost would overflow U256")?;

        Ok(SettlementSummary {
            surplus,
            gas_reimbursement,
            settled_orders: self.trades.iter().map(|t| t.order.metadata.uid).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maplit::hashmap;
    use model::order::{OrderData, OrderMetadata, OrderUid};
    use num::FromPrimitive;

    fn r(u: u128) -> BigRational {
        BigRational::from_u128(u).unwrap()
    }

    fn interaction(
        inputs: &[(H160, U256)],
        outputs: &[(H160, U256)],
        gas_used: U256,
    ) -> Arc<dyn InteractionProposal> {
        let inputs = inputs
            .iter()
            .cloned()
            .map(|(token, amount)| TokenAmount { token, amount })
            .collect();
        let outputs = outputs
            .iter()
            .cloned()
            .map(|(token, amount)| TokenAmount { token, amount })
            .collect();

        let mut interaction = MockInteractionProposal::new();
        interaction
            .expect_metadata()
            .return_const(InteractionMetadata {
                inputs,
                outputs,
                gas_used,
            });
        Arc::new(interaction)
    }

    #[test]
    fn verifies_interaction_precondition() {
        let token = H160::from_low_u64_be;
        let uid = OrderUid::from_integer;
        let native_token = token(1);

        let gas_price = 2.0;
        let external_prices = ExternalPrices::new(
            native_token,
            hashmap! { token(2) => r(100), token(3) => r(100), },
        )
        .unwrap();

        let mut proposal = SettlementProposal {
            clearing_prices: hashmap! {
                token(2) => 100.into(), token(3) => 100.into(),
            },
            trades: vec![TradedOrder {
                order: Order {
                    data: OrderData {
                        sell_token: token(2),
                        sell_amount: 60.into(),
                        buy_token: token(3),
                        buy_amount: 50.into(),
                        kind: OrderKind::Sell,
                        ..Default::default()
                    },
                    metadata: OrderMetadata {
                        uid: uid(1),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                executed_amount: 60.into(),
            }],
            ..Default::default()
        };

        // solution needs interaction to work
        assert!(proposal
            .into_settlement_summary(gas_price, &external_prices, &Default::default())
            .is_err());

        let i = interaction(&[(token(2), 60.into())], &[(token(3), 60.into())], 1.into());
        proposal.execution_plan.push(i);
        let summary = proposal
            .into_settlement_summary(gas_price, &external_prices, &Default::default())
            .unwrap();

        // gas_price * (interaction_cost + order_cost)
        assert_eq!(summary.gas_reimbursement, 132_632.into());
        assert_eq!(summary.surplus, 1_000.);
        assert_eq!(summary.settled_orders, vec![uid(1)]);
    }

    #[test]
    fn allows_flash_loans() {
        let token = H160::from_low_u64_be;
        let uid = OrderUid::from_integer;
        let native_token = token(1);

        let gas_price = 2.0;
        let external_prices = ExternalPrices::new(
            native_token,
            hashmap! { token(2) => r(100), token(3) => r(100), },
        )
        .unwrap();

        let mut proposal = SettlementProposal {
            clearing_prices: hashmap! {
                token(2) => 100.into(), token(3) => 100.into(),
            },
            trades: vec![TradedOrder {
                order: Order {
                    data: OrderData {
                        sell_token: token(2),
                        sell_amount: 60.into(),
                        buy_token: token(3),
                        buy_amount: 50.into(),
                        kind: OrderKind::Sell,
                        ..Default::default()
                    },
                    metadata: OrderMetadata {
                        uid: uid(1),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                executed_amount: 60.into(),
            }],
            ..Default::default()
        };

        let buffers = hashmap! { token(4) => 10.into() };

        let loan = interaction(
            &[(token(4), 10.into())], // inputs
            &[],                      // outputs
            1.into(),                 // gas
        );
        proposal.execution_plan.push(loan);
        let i = interaction(
            &[(token(2), 60.into())], // inputs
            &[(token(3), 60.into())], // outputs
            1.into(),                 // gas
        );
        proposal.execution_plan.push(i);

        // failing to repay the loan throws an error
        assert!(proposal
            .into_settlement_summary(gas_price, &external_prices, &buffers)
            .is_err());

        let pay_back = interaction(
            &[],                      // inputs
            &[(token(4), 10.into())], // outputs
            1.into(),                 // gas
        );
        proposal.execution_plan.push(pay_back);

        // solution can loan tokens from the settlement contract if it repays them before the end
        let summary = proposal
            .into_settlement_summary(gas_price, &external_prices, &buffers)
            .unwrap();

        // gas_price * (interaction_cost + order_cost)
        assert_eq!(summary.gas_reimbursement, 132_636.into());
        assert_eq!(summary.surplus, 1_000.);
        assert_eq!(summary.settled_orders, vec![uid(1)]);
    }
}
