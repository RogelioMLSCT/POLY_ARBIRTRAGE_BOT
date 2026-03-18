// ============================================================
// types.rs — Todas las estructuras de datos del bot
// ============================================================
//
// En Rust, defines tus tipos PRIMERO antes de usarlos.
// `#[derive(...)]` agrega funcionalidades automáticamente:
//   - Debug   → permite imprimir con {:?}
//   - Clone   → permite copiar el valor
//   - Serialize/Deserialize → convierte JSON ↔ struct
// ============================================================

use std::sync::Arc;
use serde::{Deserialize, Serialize};
use dashmap::DashMap;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

// ── Configuración del Bot ────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BotConfig {
    /// URL del WebSocket de Polymarket
    pub ws_url: String,
    /// URL de la API REST de Polymarket
    pub api_url: String,
    /// Tu API key de Polymarket (o "" para modo simulación)
   pub api_key: String,
    /// Ganancia mínima para ejecutar un trade (en dólares)
    pub min_profit_threshold: f64,
    /// Capital máximo por trade
    pub max_position_size: f64,
    /// Puerto del dashboard web
    pub dashboard_port: u16,
    /// Modo simulación (no ejecuta trades reales)
    pub dry_run: bool,
    /// Alpha para criterio de parada de Frank-Wolfe (0.9 = captura 90% del arbitraje)
    pub frank_wolfe_alpha: f64,
    /// Iteraciones máximas de Frank-Wolfe
    pub frank_wolfe_max_iter: usize,

    // ── Configuración de VaR ──────────────────────────────────
    /// Pérdida máxima permitida por día (en $)
    /// El bot se detiene automáticamente si se alcanza este límite
    pub daily_var_limit: f64,
    /// Capital total bajo gestión (para calcular VaR como % del portafolio)
    pub total_capital: f64,
    /// Ratio mínimo Ganancia/VaR para ejecutar un trade
    /// 1.5 = la ganancia debe ser al menos 1.5× el VaR del trade
    pub min_profit_var_ratio: f64,
}

impl BotConfig {
    pub fn from_env_or_default() -> Self {
        // Lee variables de entorno, o usa valores por defecto
        // Para producción: export POLYMARKET_API_KEY="tu_key"
       BotConfig {
            ws_url: std::env::var("POLYMARKET_WS_URL")
               .unwrap_or("wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string()),
           api_url: std::env::var("POLYMARKET_API_URL")
               .unwrap_or("https://clob.polymarket.com".to_string()),
           api_key: std::env::var("POLYMARKET_API_KEY")
               .unwrap_or_default(),
            min_profit_threshold: std::env::var("MIN_PROFIT")
               .unwrap_or("0.05".to_string())
               .parse().unwrap_or(0.05),
            max_position_size: std::env::var("MAX_POSITION")
               .unwrap_or("100.0".to_string())
               .parse().unwrap_or(100.0),
            dashboard_port: std::env::var("DASHBOARD_PORT")
               .unwrap_or("3000".to_string())
               .parse().unwrap_or(3000),
            dry_run: std::env::var("DRY_RUN")
               .map(|v| v == "true" || v == "1")
               .unwrap_or(true), // Por seguridad, default = simulación
            frank_wolfe_alpha: 0.9,
            frank_wolfe_max_iter: 150,
            daily_var_limit: std::env::var("DAILY_VAR_LIMIT")
               .unwrap_or("50.0".to_string())
               .parse().unwrap_or(50.0),
            total_capital: std::env::var("TOTAL_CAPITAL")
               .unwrap_or("500.0".to_string())
               .parse().unwrap_or(500.0),
            min_profit_var_ratio: std::env::var("MIN_PROFIT_VAR_RATIO")
               .unwrap_or("1.5".to_string())
               .parse().unwrap_or(1.5),
        }
    }
}

// ── Mercado de Polymarket ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    /// ID único del mercado (condition_id de Polymarket)
    pub condition_id: String,
    /// Descripción del mercado (ej: "Will Trump win PA?")
   pub question: String,
    /// Precio actual de YES (entre 0.0 y 1.0)
    pub yes_price: f64,
    /// Precio actual de NO (entre 0.0 y 1.0)
    pub no_price: f64,
    /// Volumen disponible en el order book para YES
    pub yes_volume: f64,
    /// Volumen disponible en el order book para NO
    pub no_volume: f64,
    /// Última actualización
    pub last_updated: DateTime<Utc>,
    /// Estado del mercado
    pub status: MarketStatus,
}

impl Market {
    #[allow(dead_code)]
    /// Suma de precios (debería ser ~1.0 si no hay arbitraje)
    pub fn price_sum(&self) -> f64 {
        self.yes_price + self.no_price
    }

    /// Desvíación del precio respecto a $1.00
    pub fn deviation(&self) -> f64 {
        (self.price_sum() - 1.0).abs()
    }

    /// ¿Hay arbitraje simple? (suma < 1.0 → compra ambos)
    pub fn has_simple_arbitrage(&self, threshold: f64) -> bool {
        self.price_sum() < (1.0 - threshold)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MarketStatus {
    Active,
    Resolved,
    Paused,
}

// ── Order Book ───────────────────────────────────────────────

/// Un nivel de precio en el order book
/// Ej: { price: 0.62, size: 500.0 } → hay $500 disponibles a $0.62
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct OrderLevel {
    pub price: f64,
    pub size: f64,
}

/// Order book completo de un mercado
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct OrderBook {
    /// Órdenes de compra (bids) para YES, ordenadas de mayor a menor precio
    pub yes_bids: Vec<OrderLevel>,
    /// Órdenes de venta (asks) para YES, ordenadas de menor a mayor precio
    pub yes_asks: Vec<OrderLevel>,
    pub no_bids: Vec<OrderLevel>,
    pub no_asks: Vec<OrderLevel>,
}

impl OrderBook {
    /// Calcula el VWAP (precio promedio ponderado por volumen) para una cantidad dada
    /// Esto simula el precio real que obtendrías al comprar `amount` dólares
    pub fn vwap_yes(&self, amount: f64) -> f64 {
        Self::calculate_vwap(&self.yes_asks, amount)
    }

    pub fn vwap_no(&self, amount: f64) -> f64 {
        Self::calculate_vwap(&self.no_asks, amount)
    }

    fn calculate_vwap(levels: &[OrderLevel], amount: f64) -> f64 {
        let mut remaining = amount;
        let mut total_cost = 0.0;
        let mut total_filled = 0.0;

        for level in levels {
            if remaining <= 0.0 { break; }

            // Cuánto podemos llenar en este nivel de precio
            let fill = remaining.min(level.size);
            total_cost += fill * level.price;
            total_filled += fill;
            remaining -= fill;
        }

        if total_filled > 0.0 {
            total_cost / total_filled
        } else {
            // No hay liquidez suficiente
            f64::INFINITY
        }
    }

    /// Liquidez máxima disponible en el lado de asks de YES
    pub fn yes_ask_liquidity(&self) -> f64 {
        self.yes_asks.iter().map(|l| l.size).sum()
    }

    pub fn no_ask_liquidity(&self) -> f64 {
        self.no_asks.iter().map(|l| l.size).sum()
    }
}

// ── Oportunidad de Arbitraje ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbitrageOpportunity {
    pub id: String,
    pub opportunity_type: ArbitrageType,
    /// IDs de los mercados involucrados
    pub market_ids: Vec<String>,
    /// Ganancia garantizada (en dólares por dólar invertido)
    pub guaranteed_profit: f64,
    /// Ganancia máxima posible (Bregman divergence D)
    pub max_profit: f64,
    /// Frank-Wolfe gap (g) - imprecisión en el cálculo
    pub fw_gap: f64,
    /// Tamaño óptimo de la posición
    pub position_size: f64,
    /// Las órdenes específicas a ejecutar
    pub orders: Vec<OrderToExecute>,
    pub detected_at: DateTime<Utc>,
    pub status: OpportunityStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ArbitrageType {
    /// YES + NO < $1.00 en el mismo mercado
    SimpleUnderpriced,
    /// YES + NO > $1.00 en el mismo mercado
    SimpleOverpriced,
    /// Múltiples mercados relacionados con dependencias lógicas
    Combinatorial,
    /// Rebalanceo de mercado (suma de todos los YES ≠ $1)
    MarketRebalancing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OpportunityStatus {
    Detected,
    Executing,
    Executed,
    Expired,
    Failed,
}

// ── Orden a Ejecutar ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderToExecute {
    pub market_id: String,
    pub side: OrderSide,
    pub token_id: String,
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

// ── Resultado de Trade Ejecutado ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutedTrade {
    pub opportunity_id: String,
    pub executed_at: DateTime<Utc>,
    pub orders_filled: Vec<FilledOrder>,
    pub actual_profit: f64,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilledOrder {
    pub order_id: String,
    pub market_id: String,
    pub side: OrderSide,
    pub filled_price: f64,
    pub filled_size: f64,
}

// ── Estadísticas del Bot ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BotStats {
    pub opportunities_detected: u64,
    pub opportunities_executed: u64,
    pub opportunities_failed: u64,
    pub total_profit_usd: f64,
    pub markets_monitored: usize,
    pub uptime_seconds: u64,
    pub last_opportunity_at: Option<DateTime<Utc>>,
}

// ── Estado Global Compartido ─────────────────────────────────

/// AppState es el "cerebro central" del bot.
/// Todos los módulos (feed, arbitrage, executor, dashboard) lo comparten
/// usando Arc::clone() — Rust garantiza que esto es thread-safe.
pub struct AppState {
   /// Mapa de condition_id → Market
    pub markets: DashMap<String, Market>,
    /// Mapa de opportunity_id → ArbitrageOpportunity
    pub opportunities: DashMap<String, ArbitrageOpportunity>,
    /// Mapa de trade_id → ExecutedTrade
    pub executed_trades: DashMap<String, ExecutedTrade>,
    /// Configuración global
    pub config: BotConfig,
    /// Estadísticas (usa RwLock para escrituras seguras)
    pub stats: Arc<RwLock<BotStats>>,
    /// Motor de VaR (usa RwLock porque se escribe en cada trade)
    pub var_engine: Arc<RwLock<crate::risk::VaREngine>>,
}

// ── Mensajes del WebSocket de Polymarket ────────────────────

/// Estructura del mensaje que llega por WebSocket
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsMessage {
    pub event_type: Option<String>,
    pub asset_id: Option<String>,
    pub market: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub data: Option<serde_json::Value>,
}

/// Snapshot de order book que llega por WebSocket
#[derive(Debug, Deserialize)]
pub struct BookSnapshot {
    pub asset_id: String,
    pub bids: Vec<[String; 2]>,  // [[price, size], ...]
    pub asks: Vec<[String; 2]>,
    pub timestamp: Option<String>,
}
