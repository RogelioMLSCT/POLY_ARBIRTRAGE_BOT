// ============================================================
// POLYMARKET ARBITRAGE BOT - main.rs
// ============================================================
//
// PARA PRINCIPIANTES EN RUST:
//
// `mod nombre` → declara que existe un archivo nombre.rs en src/
// `use crate::nombre::Cosa` → importa Cosa desde ese módulo
// `Arc<T>` → "Atomic Reference Count" = puntero compartido entre threads
// `async fn` → función asíncrona (no bloquea mientras espera)
// `.await` → espera el resultado de una función async
// `?` → si hay error, retórnalo automáticamente (como try/catch)
// ============================================================

mod arbitrage;
mod dashboard;
mod executor;
mod feed;
mod math;
mod risk;
mod types;

use std::sync::Arc;
use anyhow::Result;
use dashmap::DashMap;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

use crate::risk::{VaREngine, ConfidenceLevel};
use crate::types::{AppState, BotConfig, Market, ArbitrageOpportunity};

#[tokio::main]
async fn main() -> Result<()> {
   tracing_subscriber::fmt()
        .with_env_filter("polymarket_bot=info,warn")
       .init();

    info!("Iniciando Polymarket Arbitrage Bot");

   let config = BotConfig::from_env_or_default();
    info!("Config cargada: {:?}", config);

   // Inicializar motor de VaR con parámetros de la config
    let var_engine = VaREngine::new(
        ConfidenceLevel::Pct95,
        config.daily_var_limit,
        config.total_capital,
    );

    let state = Arc::new(AppState {
        markets: DashMap::new(),
        opportunities: DashMap::new(),
        executed_trades: DashMap::new(),
        var_engine: Arc::new(RwLock::new(var_engine)),
        stats: Arc::new(RwLock::new(types::BotStats::default())),
        config,
    });

    info!("VaR Engine iniciado iniciado | Límite diario: ${:.0} | Capital: ${:.0}",
         state.config.daily_var_limit, state.config.total_capital);

    tokio::select! {
        result = feed::run(Arc::clone(&state)) => {
            error!("Feed terminó inesperadamente: {:?}", result);
       }
        result = arbitrage::run(Arc::clone(&state)) => {
            error!("Arbitrage detector terminó: {:?}", result);
       }
        result = executor::run(Arc::clone(&state)) => {
            error!("Executor terminó: {:?}", result);
       }
        result = dashboard::run(Arc::clone(&state)) => {
            error!("Dashboard terminó: {:?}", result);
       }
    }

    warn!("Bot detenido.");
    Ok(())
}
