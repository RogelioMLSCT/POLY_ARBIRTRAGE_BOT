// ============================================================
// executor.rs — Ejecutor de órdenes
// ============================================================
//
// CRÍTICO: Este módulo maneja dinero real.
//
// Lógica de seguridad:
// 1. Verificar que la oportunidad sigue siendo válida
// 2. Verificar liquidez actual en el order book
// 3. Calcular slippage esperado
// 4. Solo ejecutar si profit garantizado > threshold
// 5. En dry_run: simular sin ejecutar
//
// El mayor riesgo es ejecución NO-ATÓMICA:
// Si compras YES pero NO ya no está disponible al mismo precio,
// quedas expuesto. Por eso verificamos ANTES de ejecutar.
// ============================================================

use std::sync::Arc;
use anyhow::{Result, anyhow};
use chrono::Utc;
use tracing::{info, warn, error, debug};

use crate::types::{
    AppState, ArbitrageOpportunity, ExecutedTrade,
    FilledOrder, OpportunityStatus, OrderSide,
};
use crate::risk::{
    ConfidenceLevel, DailyLimitCheck, ExecutionType,
    OpportunityRiskProfile, PnLRecord, RiskRecommendation,
};

/// Corre el loop del ejecutor
/// Revisa oportunidades detectadas y las ejecuta si siguen siendo válidas
pub async fn run(state: Arc<AppState>) -> Result<()> {
    info!("Iniciando ejecutor de órdenes (dry_run={})", state.config.dry_run);

   if state.config.dry_run {
        warn!("MODO SIMULACION ACTIVO - No se ejecutarán trades reales");
       warn!("   Para trading real: export DRY_RUN=false");
   }

    let check_interval = tokio::time::Duration::from_millis(50); // Revisar cada 50ms
    let mut interval = tokio::time::interval(check_interval);

    loop {
        interval.tick().await;

        // Obtener oportunidades pendientes (más rentables primero)
        let mut pending: Vec<ArbitrageOpportunity> = state.opportunities.iter()
            .filter(|entry| matches!(entry.value().status, OpportunityStatus::Detected))
            .map(|entry| entry.value().clone())
            .collect();

        // Ordenar por ganancia garantizada (mayor primero)
        pending.sort_by(|a, b| {
            b.guaranteed_profit.partial_cmp(&a.guaranteed_profit)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Ejecutar las top 3 oportunidades por iteración
        for opportunity in pending.iter().take(3) {
            if let Err(e) = process_opportunity(&state, opportunity).await {
                warn!("Error procesando oportunidad {}: {}", &opportunity.id[..8], e);
           }
        }
    }
}

/// Procesa una oportunidad: valida, evalúa VaR y ejecuta si es seguro
async fn process_opportunity(
    state: &Arc<AppState>,
    opportunity: &ArbitrageOpportunity,
) -> Result<()> {
    // Marcar como "ejecutando" para evitar ejecución doble
   if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
        if !matches!(opp.status, OpportunityStatus::Detected) {
            return Ok(());
        }
        opp.status = OpportunityStatus::Executing;
    }

    // ── Validación de Precios ────────────────────────────────
    let current_profit = validate_opportunity_still_valid(state, opportunity);
    if current_profit < state.config.min_profit_threshold {
        warn!("Expirado {} (profit actual: ${:.4})", &opportunity.id[..8], current_profit);
       if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
            opp.status = OpportunityStatus::Expired;
        }
        return Ok(());
    }

    // ── Validación de Liquidez ───────────────────────────────
    let min_liquidity = check_liquidity(state, opportunity);
    if min_liquidity < opportunity.position_size * 0.8 {
        warn!("Liquidez insuficiente para {}: ${:.2} < ${:.2}",
             &opportunity.id[..8], min_liquidity, opportunity.position_size);
        if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
            opp.status = OpportunityStatus::Expired;
        }
        return Ok(());
    }

    // ══════════════════════════════════════════════════════════
    // ── GATE DE VaR ─────────────────────────────────────────
    // Antes de ejecutar cualquier trade, el VaR debe aprobarlo.
    // Si el riesgo es demasiado alto, el trade se descarta.
    // ══════════════════════════════════════════════════════════

    // Obtener precios actuales del mercado para calcular el perfil de riesgo
    let (yes_price, no_price, yes_vol, no_vol) = {
        let market_id = opportunity.market_ids.first()
            .ok_or_else(|| anyhow!("Sin market_id"))?;
       match state.markets.get(market_id) {
            Some(m) => (m.yes_price, m.no_price, m.yes_volume, m.no_volume),
            None => {
                warn!("Mercado {} no encontrado en estado", market_id);
               if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
                    opp.status = OpportunityStatus::Expired;
                }
                return Ok(());
            }
        }
    };

    // Construir perfil de riesgo para esta oportunidad
    let risk_profile = OpportunityRiskProfile::from_opportunity(
        &opportunity.id,
        opportunity.guaranteed_profit,
        yes_price,
        no_price,
        opportunity.position_size,
        yes_vol,
        no_vol,
    );

    // Calcular VaR de ejecución
    let var_result = {
        let var_engine = state.var_engine.read().await;
        var_engine.execution_var(&risk_profile, ConfidenceLevel::Pct95)
    };

    // ── Verificar límite diario ──────────────────────────────
    let daily_check = {
        let var_engine = state.var_engine.read().await;
        var_engine.check_daily_limit(var_result.var)
    };

    if daily_check.is_blocked() {
        if let DailyLimitCheck::Blocked { reason, .. } = &daily_check {
            warn!("LIMITE DIARIO DE VaR ALCANZADO: DE VaR ALCANZADO: {}", reason);
       }
        if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
            opp.status = OpportunityStatus::Expired;
        }
        return Ok(());
    }

    // ── Evaluar recomendación del motor de VaR ───────────────
    let final_position_size = match &var_result.recommendation {
        RiskRecommendation::Execute => {
            debug!("VaR aprueba trade {} | VaR=${:.4} | EV=${:.4} | P/VaR={:.2}x",
                  &opportunity.id[..8], var_result.var,
                   var_result.expected_value, var_result.profit_var_ratio);
            opportunity.position_size
        }

        RiskRecommendation::ExecuteReduced { suggested_size_pct } => {
            let reduced = opportunity.position_size * suggested_size_pct;
            warn!("VaR reduce posición {} al {:.0}% (${:.2} → ${:.2})",
                 &opportunity.id[..8], suggested_size_pct * 100.0,
                  opportunity.position_size, reduced);
            reduced
        }

        RiskRecommendation::Skip { reason } => {
            warn!("VaR descarta trade {}: {}", &opportunity.id[..8], reason);
           if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
                opp.status = OpportunityStatus::Expired;
            }
            return Ok(());
        }
    };

    // Verificar ratio mínimo Profit/VaR
    if var_result.profit_var_ratio < state.config.min_profit_var_ratio
        && var_result.var > 0.0
    {
        warn!("Ratio P/VaR insuficiente para {}: {:.2}x < {:.2}x requerido",
             &opportunity.id[..8],
              var_result.profit_var_ratio,
              state.config.min_profit_var_ratio);
        if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
            opp.status = OpportunityStatus::Expired;
        }
        return Ok(());
    }

    info!(
        " Ejecutando {} | Profit: ${:.4} | VaR(95%): ${:.4} | CVaR: ${:.4} | \
         EV: ${:.4} | Fill prob: {:.1}% | Pos: ${:.2}{}",
       &opportunity.id[..8],
        opportunity.guaranteed_profit,
        var_result.var,
        var_result.cvar,
        var_result.expected_value,
        var_result.fill_probability * 100.0,
        final_position_size,
        if state.config.dry_run { " (SIMULADO)" } else { " (REAL)" }
   );

    // ── Ejecución ────────────────────────────────────────────
    let result = if state.config.dry_run {
        simulate_execution(opportunity, final_position_size)
    } else {
        execute_on_polymarket(state, opportunity).await
    };

    // ── Registrar Resultado y actualizar VaR ─────────────────
    match result {
        Ok(trade) => {
            // Registrar P&L en el motor de VaR
            {
                let mut var_engine = state.var_engine.write().await;
                var_engine.record_trade(PnLRecord {
                    trade_id: trade.opportunity_id.clone(),
                    pnl: trade.actual_profit,
                    capital_at_risk: risk_profile.capital_committed,
                    execution_type: if trade.success {
                        ExecutionType::FullFill
                    } else {
                        ExecutionType::PartialFill {
                            legs_filled: trade.orders_filled.len(),
                            legs_total: opportunity.orders.len(),
                        }
                    },
                    timestamp: Utc::now(),
                });
            }

            info!("Trade completado: {} | Profit: ${:.4}",
                 &opportunity.id[..8], trade.actual_profit);

            if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
                opp.status = OpportunityStatus::Executed;
            }
            let trade_id = trade.opportunity_id.clone();
            state.executed_trades.insert(trade_id, trade);

            let mut stats = state.stats.write().await;
            stats.opportunities_executed += 1;
            stats.total_profit_usd += opportunity.guaranteed_profit;
        }
        Err(e) => {
            error!("Trade fallido {}: {}", &opportunity.id[..8], e);

           // Registrar pérdida en VaR (worst case: perdimos el capital comprometido)
            {
                let mut var_engine = state.var_engine.write().await;
                var_engine.record_trade(PnLRecord {
                    trade_id: opportunity.id.clone(),
                    pnl: -risk_profile.worst_case_single_leg_loss * 0.1, // Pérdida parcial estimada
                    capital_at_risk: risk_profile.capital_committed,
                    execution_type: ExecutionType::NoFill,
                    timestamp: Utc::now(),
                });
            }

            if let Some(mut opp) = state.opportunities.get_mut(&opportunity.id) {
                opp.status = OpportunityStatus::Failed;
            }
            let mut stats = state.stats.write().await;
            stats.opportunities_failed += 1;
        }
    }

    Ok(())
}

/// Verifica si una oportunidad sigue siendo válida con los precios actuales
fn validate_opportunity_still_valid(
    state: &AppState,
    opportunity: &ArbitrageOpportunity,
) -> f64 {
    // Para arbitraje simple, verificar que los precios siguen teniendo la misma dirección
    let total_current_price: f64 = opportunity.orders.iter()
        .filter_map(|order| {
            state.markets.get(&order.market_id)
                .map(|market| match order.side {
                    OrderSide::Buy => {
                        // Para compras, el precio relevante es el ask actual
                        if order.token_id.ends_with("_YES") {
                           market.yes_price
                        } else {
                            market.no_price
                        }
                    }
                    OrderSide::Sell => {
                        // Para ventas, el precio relevante es el bid actual
                        if order.token_id.ends_with("_YES") {
                           market.yes_price
                        } else {
                            market.no_price
                        }
                    }
                })
        })
        .sum();

    // Para tipo Underpriced: ganancia = 1 - total_price (si < 1.0)
    // Para tipo Overpriced: ganancia = total_price - 1 (si > 1.0)
    let n_orders = opportunity.orders.len() as f64;
    if n_orders == 0.0 { return 0.0; }

    // Calcular ganancia según tipo
    match opportunity.opportunity_type {
        crate::types::ArbitrageType::SimpleUnderpriced => {
            (1.0 - total_current_price).max(0.0) * opportunity.position_size
        }
        crate::types::ArbitrageType::SimpleOverpriced => {
            (total_current_price - 1.0).max(0.0) * opportunity.position_size
        }
        _ => opportunity.guaranteed_profit, // Para otros tipos, confiar en el cálculo previo
    }
}

/// Verifica liquidez disponible para todas las órdenes
fn check_liquidity(state: &AppState, opportunity: &ArbitrageOpportunity) -> f64 {
    opportunity.orders.iter()
        .filter_map(|order| {
            state.markets.get(&order.market_id).map(|market| {
                if order.token_id.ends_with("_YES") {
                   market.yes_volume
                } else {
                    market.no_volume
                }
            })
        })
        .fold(f64::INFINITY, f64::min) // Mínimo de todas las liquideces
}

/// Simula ejecución (modo dry_run)
fn simulate_execution(opportunity: &ArbitrageOpportunity, position_size: f64) -> Result<ExecutedTrade> {
    let filled_orders: Vec<FilledOrder> = opportunity.orders.iter()
        .map(|order| FilledOrder {
            order_id: format!("sim_{}", uuid::Uuid::new_v4()),
           market_id: order.market_id.clone(),
            side: order.side.clone(),
            filled_price: order.price,
            filled_size: position_size,
        })
        .collect();

    Ok(ExecutedTrade {
        opportunity_id: opportunity.id.clone(),
        executed_at: Utc::now(),
        orders_filled: filled_orders,
        actual_profit: opportunity.guaranteed_profit,
        success: true,
        error: None,
    })
}

/// Ejecuta órdenes reales en Polymarket via API
/// IMPORTANTE: Requiere API key válida y USDC en la wallet
async fn execute_on_polymarket(
    state: &Arc<AppState>,
    opportunity: &ArbitrageOpportunity,
) -> Result<ExecutedTrade> {
    let client = reqwest::Client::new();
    let mut filled_orders = vec![];

    // Ejecutar todas las órdenes en el menor tiempo posible
    // NOTA: En producción real, se harían todas en paralelo (tokio::join!)
    // para minimizar el riesgo de ejecución parcial
    for order in &opportunity.orders {
        let order_payload = serde_json::json!({
            "tokenID": order.token_id,
           "price": order.price,
           "size": order.size,
           "side": match order.side {
               OrderSide::Buy => "BUY",
               OrderSide::Sell => "SELL",
           },
            "type": "LIMIT",
           "timeInForce": "FOK", // Fill-Or-Kill: se cancela si no llena completo
        });

        let response = client
            .post(format!("{}/order", state.config.api_url))
           .header("Authorization", format!("Bearer {}", state.config.api_key))
           .json(&order_payload)
            .send()
            .await
            .map_err(|e| anyhow!("HTTP error: {}", e))?;

       if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow!("API error: {}", error_text));
       }

        let result: serde_json::Value = response.json().await?;

        filled_orders.push(FilledOrder {
            order_id: result["orderID"].as_str().unwrap_or("unknown").to_string(),
           market_id: order.market_id.clone(),
            side: order.side.clone(),
            filled_price: result["price"].as_f64().unwrap_or(order.price),
           filled_size: result["sizeMatched"].as_f64().unwrap_or(order.size),
       });

        debug!("Orden enviada: {:?}", result);
    }

    // Calcular profit real basado en precios de fill
    let total_cost: f64 = filled_orders.iter()
        .map(|f| f.filled_price * f.filled_size)
        .sum();

    let actual_profit = (opportunity.position_size - total_cost).max(0.0);

    Ok(ExecutedTrade {
        opportunity_id: opportunity.id.clone(),
        executed_at: Utc::now(),
        orders_filled: filled_orders,
        actual_profit,
        success: true,
        error: None,
    })
}
