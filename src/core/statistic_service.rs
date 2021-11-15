use super::{
    nothing_to_do,
    orders::{event::OrderEventType, order::ClientOrderId},
};
use anyhow::{Context, Result};
use futures::FutureExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::{
    exchanges::{
        common::{Amount, Price, TradePlaceAccount},
        events::ExchangeEvent,
    },
    infrastructure::spawn_future,
};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TradePlaceAccountStatistic {
    opened_orders_count: u64,
    canceled_orders_count: u64,
    partially_filled_orders_count: u64,
    fully_filled_orders_count: u64,
    // Calculated only for completely filled orders
    summary_filled_amount: Amount,
    // Calculated only for completely filled orders
    summary_commission: Amount,
}

impl TradePlaceAccountStatistic {
    fn register_created_order(&mut self) {
        self.opened_orders_count += 1;
    }

    fn register_canceled_order(&mut self) {
        self.canceled_orders_count += 1;
    }

    fn increment_partially_filled_orders(&mut self) {
        self.partially_filled_orders_count += 1;
    }

    fn decrement_partially_filled_orders(&mut self) {
        if self.partially_filled_orders_count == 0 {
            log::error!("Unable to decrement partially filled orders count, because there are no more partially filled orders");
        } else {
            self.partially_filled_orders_count -= 1;
        }
    }

    fn increment_completely_filled_orders(&mut self) {
        self.fully_filled_orders_count += 1;
    }

    fn add_summary_filled_amount(&mut self, filled_amount: Amount) {
        self.summary_filled_amount += filled_amount;
    }

    fn add_summary_commission(&mut self, commission: Price) {
        self.summary_commission += commission;
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DispositionExecutorStatistic {
    skipped_events_amount: u64,
}

impl DispositionExecutorStatistic {
    fn new(skipped_events_amount: u64) -> Self {
        Self {
            skipped_events_amount,
        }
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub(crate) struct StatisticServiceState {
    trade_place_stats: RwLock<HashMap<TradePlaceAccount, TradePlaceAccountStatistic>>,
    disposition_executor_stats: Mutex<DispositionExecutorStatistic>,
}

impl StatisticServiceState {
    fn new() -> Self {
        Self {
            trade_place_stats: Default::default(),
            disposition_executor_stats: Default::default(),
        }
    }

    pub(crate) fn register_created_order(&self, trade_place_account: TradePlaceAccount) {
        self.trade_place_stats
            .write()
            .entry(trade_place_account)
            .or_default()
            .register_created_order();
    }

    pub(crate) fn register_canceled_order(&self, trade_place_account: TradePlaceAccount) {
        self.trade_place_stats
            .write()
            .entry(trade_place_account)
            .or_default()
            .register_canceled_order();
    }

    pub(crate) fn register_partially_filled_order(&self, trade_place_account: TradePlaceAccount) {
        self.trade_place_stats
            .write()
            .entry(trade_place_account)
            .or_default()
            .increment_partially_filled_orders();
    }

    fn decrement_partially_filled_orders(&self, trade_place_account: TradePlaceAccount) {
        self.trade_place_stats
            .write()
            .entry(trade_place_account)
            .or_default()
            .decrement_partially_filled_orders();
    }

    pub(crate) fn register_completely_filled_order(&self, trade_place_account: TradePlaceAccount) {
        self.trade_place_stats
            .write()
            .entry(trade_place_account)
            .or_default()
            .increment_completely_filled_orders();
    }

    pub(crate) fn register_filled_amount(
        &self,
        trade_place_account: TradePlaceAccount,
        filled_amount: Amount,
    ) {
        self.trade_place_stats
            .write()
            .entry(trade_place_account)
            .or_default()
            .add_summary_filled_amount(filled_amount);
    }

    pub(crate) fn register_commission(
        &self,
        trade_place_account: TradePlaceAccount,
        commission: Price,
    ) {
        self.trade_place_stats
            .write()
            .entry(trade_place_account)
            .or_default()
            .add_summary_commission(commission);
    }

    pub(crate) fn register_skipped_event(&self) {
        (*self.disposition_executor_stats.lock()).skipped_events_amount += 1;
    }
}

#[derive(Default, Debug)]
pub struct StatisticService {
    pub(crate) statistic_service_state: StatisticServiceState,
    partially_filled_orders: Mutex<HashSet<ClientOrderId>>,
}

impl StatisticService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            statistic_service_state: Default::default(),
            partially_filled_orders: Default::default(),
        })
    }

    pub(crate) fn register_created_order(&self, trade_place_account: TradePlaceAccount) {
        self.statistic_service_state
            .register_created_order(trade_place_account);
    }

    pub(crate) fn register_canceled_order(
        &self,
        trade_place_account: TradePlaceAccount,
        client_order_id: &ClientOrderId,
    ) {
        self.statistic_service_state
            .register_canceled_order(trade_place_account);

        self.remove_filled_order_if_exist(trade_place_account, &client_order_id);
    }

    pub(crate) fn register_partially_filled_order(
        &self,
        trade_place_account: TradePlaceAccount,
        client_order_id: &ClientOrderId,
    ) {
        let mut partially_filled_orders = self.partially_filled_orders.lock();

        if !(*partially_filled_orders).contains(&client_order_id) {
            self.statistic_service_state
                .register_partially_filled_order(trade_place_account);
            let _ = partially_filled_orders.insert(client_order_id.clone());
        }
    }

    pub(crate) fn register_completely_filled_order(
        &self,
        trade_place_account: TradePlaceAccount,
        client_order_id: &ClientOrderId,
        filled_amount: Amount,
        commission: Amount,
    ) {
        self.statistic_service_state
            .register_completely_filled_order(trade_place_account);

        self.remove_filled_order_if_exist(trade_place_account, client_order_id);

        self.statistic_service_state
            .register_filled_amount(trade_place_account, filled_amount);

        self.statistic_service_state
            .register_commission(trade_place_account, commission);
    }

    fn remove_filled_order_if_exist(
        &self,
        trade_place_account: TradePlaceAccount,
        client_order_id: &ClientOrderId,
    ) {
        let mut partially_filled_orders = self.partially_filled_orders.lock();

        if (*partially_filled_orders).contains(&client_order_id) {
            self.statistic_service_state
                .decrement_partially_filled_orders(trade_place_account);
            let _ = partially_filled_orders.remove(client_order_id);
        }
    }

    pub(crate) fn register_skipped_event(&self) {
        self.statistic_service_state.register_skipped_event();
    }
}

pub struct StatisticEventHandler {
    pub(crate) stats: Arc<StatisticService>,
}

impl StatisticEventHandler {
    pub fn new(
        events_receiver: broadcast::Receiver<ExchangeEvent>,
        stats: Arc<StatisticService>,
    ) -> Arc<Self> {
        let statistic_event_handler = Arc::new(Self { stats });

        let action = statistic_event_handler.clone().start(events_receiver);
        spawn_future("Start statistic service", true, action.boxed());

        statistic_event_handler
    }

    pub async fn start(
        self: Arc<Self>,
        mut events_receiver: broadcast::Receiver<ExchangeEvent>,
    ) -> Result<()> {
        loop {
            let event = events_receiver
                .recv()
                .await
                .context("Error during receiving event in DispositionExecutor::start()")?;
            // There is no need to stop StatisticEventHandler via CancellationToken now
            // Better to collect all statistics, even events occur during graceful_shutdown
            // But then statistic future will work until tokio runtime is up

            self.handle_event(event)?;
        }
    }

    fn handle_event(&self, event: ExchangeEvent) -> Result<()> {
        match event {
            ExchangeEvent::OrderEvent(order_event) => {
                let trade_place_account = TradePlaceAccount::new(
                    order_event.order.exchange_account_id(),
                    order_event.order.currency_pair(),
                );
                match order_event.event_type {
                    OrderEventType::CreateOrderSucceeded => {
                        self.stats.register_created_order(trade_place_account);
                    }
                    OrderEventType::CancelOrderSucceeded => {
                        let client_order_id = order_event.order.client_order_id();
                        self.stats
                            .register_canceled_order(trade_place_account, &client_order_id);
                    }
                    OrderEventType::OrderFilled { cloned_order } => {
                        self.stats.register_partially_filled_order(
                            trade_place_account,
                            &cloned_order.header.client_order_id,
                        );
                    }
                    OrderEventType::OrderCompleted { cloned_order } => {
                        let commission = cloned_order
                            .fills
                            .fills
                            .iter()
                            .map(|fill| fill.commission_amount())
                            .sum();

                        let filled_amount = cloned_order.fills.filled_amount;

                        self.stats.register_completely_filled_order(
                            trade_place_account,
                            &cloned_order.header.client_order_id,
                            filled_amount,
                            commission,
                        );
                    }
                    _ => nothing_to_do(),
                }
            }
            _ => nothing_to_do(),
        }

        Ok(())
    }
}
