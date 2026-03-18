// ============================================================
// arbitrage.rs — Motor de detección de arbitraje
// ============================================================
//
// Este módulo escanea todos los mercados cada ~100ms buscando:
//
// 1. ARBITRAJE SIMPLE: YES + NO ≠ $1.00
//    Si suma < 1: comprar ambos → ganancia garantizada
//    Si suma > 1: vender ambos → ganancia garantizada
//
// 2. ARBITRAJE DE REBALANCEO: En mercados con múltiples outcomes
//    (ej: margen de victoria: 0-5%, 5-10%, 10%+)
//    Si suma de todos los YES ≠ $1.00 → arbitraje
//
// 3. ARBITRAJE COMBINATORIAL: Entre mercados relacionados
//    (implementación simplificada sin Gurobi)
// ============================================================

use std::sync::Arc;
use anyhow::Result;
use chrono::Utc;
use tracing::{info, debug, warn};
use uuid::Uuid;

use crate::types::{
    AppState, ArbitrageOpportunity, ArbitrageType,
    Market, OpportunityStatus, OrderSide, OrderToExecute,
};
use crate::math::{
    analyze_simple_arbitrage, frank_wolfe_project,
    kelly_position_size, MarketConstraints, SimpleArbitrageType,
};

/// Corre el loop de detección de arbitraje
/// Escanea todos los mercados cada 100ms
pub async fn run(state: Arc<AppState>) -> Result<()> {
    info!("Iniciando detector de arbitraje...");

   let scan_interval = tokio::time::Duration::from_millis(100);
    let mut interval = tokio::time::interval(scan_interval);

    loop {
        interval.tick().await;

        // Escanear todos los mercados activos
        if let Err(e) = scan_all_markets(&state).await {
            warn!("Error en scan de mercados: {}", e);
       }

        // Limpiar oportunidades viejas (>5 minutos)
        cleanup_stale_opportunities(&state);
    }
}

/// Escanea todos los mercados buscando arbitraje
async fn scan_all_markets(state: &Arc<AppState>) -> Result<()> {
    let markets: Vec<Market> = state.markets.iter()
        .map(|entry| entry.value().clone())
        .collect();

    let min_profit = state.config.min_profit_threshold;

    debug!("Escaneando {} mercados...", markets.len());

   for market in &markets {
        // ── Tipo 1: Arbitraje Simple YES/NO ─────────────────
        check_simple_arbitrage(state, market, min_profit).await;
    }

    // Actualizar estadísticas
    let opp_count = state.opportunities.len();
    if opp_count > 0 {
        debug!("{} oportunidades activas detectadas", opp_count);
   }

    Ok(())
}

/// Chequea arbitraje simple en un mercado YES/NO
async fn check_simple_arbitrage(
    state: &Arc<AppState>,
    market: &Market,
    min_profit: f64,
) {
    // Análisis matemático básico
    let analysis = analyze_simple_arbitrage(
        &market.condition_id,
        market.yes_price,
        market.no_price,
        min_profit,
    );

    if matches!(analysis.arbitrage_type, SimpleArbitrageType::None) {
        return; // Sin arbitraje, ignorar
    }

    // Calcular tamaño óptimo de posición
    let available_liquidity = market.yes_volume.min(market.no_volume);
    let fill_probability = estimate_fill_probability(available_liquidity, min_profit);

    let position_size = kelly_position_size(
        analysis.gross_profit_per_dollar,
        fill_probability,
        state.config.max_position_size,
        available_liquidity,
    );

    if position_size < 1.0 {
        debug!("Posición demasiado pequeña (${:.2}) para market {}", 
              position_size, &market.condition_id[..8.min(market.condition_id.len())]);
        return;
    }

    // ── Frank-Wolfe para profit garantizado exacto ───────────
    let theta = vec![market.yes_price, market.no_price];
    let constraints = MarketConstraints::simple_yes_no();

    let fw_result = frank_wolfe_project(
        &theta,
        &constraints,
        state.config.frank_wolfe_alpha,
        state.config.frank_wolfe_max_iter,
    );

    let guaranteed_profit = fw_result.guaranteed_profit * position_size;

    if guaranteed_profit < min_profit {
        return; // Ganancia insuficiente después del cálculo exacto
    }

    // ── Construir órdenes a ejecutar ─────────────────────────
    let orders = build_orders_for_simple_arbitrage(market, &analysis, position_size);

    // ── Crear oportunidad ────────────────────────────────────
    let opp_id = Uuid::new_v4().to_string();

    let arb_type = match analysis.arbitrage_type {
        SimpleArbitrageType::Underpriced { .. } => ArbitrageType::SimpleUnderpriced,
        SimpleArbitrageType::Overpriced { .. } => ArbitrageType::SimpleOverpriced,
        SimpleArbitrageType::None => return,
    };

    let opportunity = ArbitrageOpportunity {
        id: opp_id.clone(),
        opportunity_type: arb_type,
        market_ids: vec![market.condition_id.clone()],
        guaranteed_profit,
        max_profit: fw_result.divergence * position_size,
        fw_gap: fw_result.fw_gap,
        position_size,
        orders,
        detected_at: Utc::now(),
        status: OpportunityStatus::Detected,
    };

    info!(
        " ARBITRAJE DETECTADO: {} | Tipo: YES={:.3}+NO={:.3}={:.3} | \
         Ganancia garantizada: ${:.4} | Posición: ${:.2}",
       &market.condition_id[..8.min(market.condition_id.len())],
        market.yes_price,
        market.no_price,
        analysis.price_sum,
        guaranteed_profit,
        position_size,
    );

    // Guardar en estado compartido
    state.opportunities.insert(opp_id, opportunity);

    // Actualizar estadísticas
    let mut stats = state.stats.write().await;
    stats.opportunities_detected += 1;
    stats.last_opportunity_at = Some(Utc::now());
}

/// Construye las órdenes específicas para arbitraje simple
fn build_orders_for_simple_arbitrage(
    market: &Market,
    analysis: &crate::math::SimpleArbitrageAnalysis,
    position_size: f64,
) -> Vec<OrderToExecute> {
    match &analysis.arbitrage_type {
        SimpleArbitrageType::Underpriced { .. } => {
            // Comprar tanto YES como NO
            // Costo total: yes_price + no_price < $1.00
            // Ganancia garantizada cuando se resuelva
            vec![
                OrderToExecute {
                    market_id: market.condition_id.clone(),
                    side: OrderSide::Buy,
                    token_id: format!("{}_YES", market.condition_id),
                   price: market.yes_price,
                    size: position_size,
                },
                OrderToExecute {
                    market_id: market.condition_id.clone(),
                    side: OrderSide::Buy,
                    token_id: format!("{}_NO", market.condition_id),
                   price: market.no_price,
                    size: position_size,
                },
            ]
        }
        SimpleArbitrageType::Overpriced { .. } => {
            // Vender tanto YES como NO
            // Recibes: yes_price + no_price > $1.00
            // Pagas $1.00 cuando se resuelva → ganancia garantizada
            vec![
                OrderToExecute {
                    market_id: market.condition_id.clone(),
                    side: OrderSide::Sell,
                    token_id: format!("{}_YES", market.condition_id),
                   price: market.yes_price,
                    size: position_size,
                },
                OrderToExecute {
                    market_id: market.condition_id.clone(),
                    side: OrderSide::Sell,
                    token_id: format!("{}_NO", market.condition_id),
                    price: market.no_price,
                    size: position_size,
                },
            ]
        }
        SimpleArbitrageType::None => vec![],
    }
}

/// Estima la probabilidad de que ambas órdenes se ejecuten completamente
/// basado en la liquidez disponible
fn estimate_fill_probability(liquidity: f64, position_size: f64) -> f64 {
    if liquidity <= 0.0 { return 0.0; }
    if position_size <= 0.0 { return 1.0; }

    // Más liquidez disponible → más probable el fill completo
    let ratio = liquidity / position_size;
    1.0 - (-ratio).exp() // Función sigmoide asimétrica
}

/// Elimina oportunidades viejas del mapa
fn cleanup_stale_opportunities(state: &AppState) {
    let cutoff = Utc::now() - chrono::Duration::minutes(5);

    state.opportunities.retain(|_, opp| {
        // Mantener si:
        // - Es reciente (menos de 5 minutos)
        // - O está en proceso de ejecución
        opp.detected_at > cutoff
            || matches!(opp.status, OpportunityStatus::Executing)
    });
}
