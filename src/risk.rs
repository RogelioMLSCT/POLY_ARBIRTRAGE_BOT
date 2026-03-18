// ============================================================
// risk.rs — Value at Risk (VaR) para el Bot de Polymarket
// ============================================================
//
// ¿POR QUÉ VaR EN ARBITRAJE?
//
// El arbitraje matemático elimina el riesgo del MERCADO
// (no importa si YES o NO gana), pero NO elimina el riesgo
// de EJECUCIÓN. Hay 3 fuentes de riesgo real:
//
// 1. RIESGO DE PIERNA PARCIAL
//    Compras YES, pero NO sube antes de que tu orden llene.
//    Ya no es arbitraje — es una posición direccional expuesta.
//
// 2. RIESGO DE LIQUIDEZ
//    El order book no tiene suficiente volumen al precio cotizado.
//    Tu VWAP real es peor que el esperado.
//
// 3. RIESGO DE CONCENTRACIÓN
//    Tienes demasiado capital en pocos mercados correlacionados.
//    Si todos fallan juntos (ej: todos son mercados electorales),
//    pierdes mucho de una vez.
//
// VaR responde: "En el peor X% de los casos, ¿cuánto pierdo?"
//
// MÉTODOS IMPLEMENTADOS:
//  - VaR Histórico (más simple, usa trades pasados)
//   - VaR Paramétrico (asume distribución normal)
//   - CVaR / Expected Shortfall (promedio de pérdidas en la cola)
//   - VaR de Ejecución (específico para riesgo de pierna parcial)
// ============================================================

use std::collections::VecDeque;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::debug;

// ── Parámetros de VaR ────────────────────────────────────────

/// Niveles de confianza estándar para VaR
/// VaR(95%) = pérdida máxima en el 95% de los casos
/// VaR(99%) = pérdida máxima en el 99% de los casos
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ConfidenceLevel {
    Pct90,  // 90% — más permisivo
    Pct95,  // 95% — estándar de la industria
    Pct99,  // 99% — conservador
    Pct999, // 99.9% — muy conservador
}

impl ConfidenceLevel {
    /// Retorna el percentil de pérdida (ej: 95% → tomar el 5% peor)
    pub fn alpha(&self) -> f64 {
        match self {
            Self::Pct90  => 0.10,
            Self::Pct95  => 0.05,
            Self::Pct99  => 0.01,
            Self::Pct999 => 0.001,
        }
    }

    /// Z-score para distribución normal
    /// (cuántas desviaciones estándar corresponden a este percentil)
    pub fn z_score(&self) -> f64 {
        match self {
            Self::Pct90  => 1.282,
            Self::Pct95  => 1.645,
            Self::Pct99  => 2.326,
            Self::Pct999 => 3.090,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Pct90  => "90%",
           Self::Pct95  => "95%",
           Self::Pct99  => "99%",
           Self::Pct999 => "99.9%",
       }
    }
}

// ── Registro de P&L histórico ────────────────────────────────

/// Un registro de pérdida/ganancia de un trade pasado
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnLRecord {
    pub trade_id: String,
    /// P&L realizado (positivo = ganancia, negativo = pérdida)
    pub pnl: f64,
    /// Capital en riesgo en ese trade
    pub capital_at_risk: f64,
    /// Tipo de resultado: ejecución completa, parcial, fallida
    pub execution_type: ExecutionType,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ExecutionType {
    /// Todas las órdenes llenaron al precio esperado
    FullFill,
    /// Solo llenó una pierna (riesgo de exposición direccional)
    PartialFill { legs_filled: usize, legs_total: usize },
    /// Ninguna orden llenó
    NoFill,
    /// Trade simulado (dry_run)
    Simulated,
}

// ── Motor de VaR ─────────────────────────────────────────────

/// Motor central de cálculo de VaR
#[derive(Debug)]
#[allow(dead_code)]
pub struct VaREngine {
    /// Historial de P&L (ventana deslizante)
    pnl_history: VecDeque<PnLRecord>,
    /// Tamaño máximo del historial
    max_history: usize,
    /// Nivel de confianza por defecto
    pub confidence_level: ConfidenceLevel,
    /// Límite de VaR diario (pérdida máxima permitida por día en $)
    pub daily_var_limit: f64,
    /// P&L acumulado del día actual
    pub daily_pnl: f64,
    /// Capital total bajo gestión
    pub total_capital: f64,
    /// Fecha del día actual (para resetear daily_pnl)
    current_day: chrono::NaiveDate,
}

impl VaREngine {
    pub fn new(
        confidence_level: ConfidenceLevel,
        daily_var_limit: f64,
        total_capital: f64,
    ) -> Self {
        VaREngine {
            pnl_history: VecDeque::with_capacity(500),
            max_history: 500, // Últimos 500 trades para VaR histórico
            confidence_level,
            daily_var_limit,
            daily_pnl: 0.0,
            total_capital,
            current_day: Utc::now().date_naive(),
        }
    }

    /// Registrar el resultado de un trade
    pub fn record_trade(&mut self, record: PnLRecord) {
        // Resetear P&L diario si cambió el día
        let today = Utc::now().date_naive();
        if today != self.current_day {
            debug!("Nuevo dia — reseteando P&L diario (anterior: ${:.2})", self.daily_pnl);
           self.daily_pnl = 0.0;
            self.current_day = today;
        }

        self.daily_pnl += record.pnl;

        // Mantener ventana deslizante
        if self.pnl_history.len() >= self.max_history {
            self.pnl_history.pop_front();
        }
        self.pnl_history.push_back(record);
    }

    // ── VaR Histórico ────────────────────────────────────────

    /// VaR Histórico: ordena P&L histórico y toma el percentil inferior
    ///
    /// Ejemplo con 100 trades y confianza 95%:
    ///   - Ordena los 100 P&L de menor a mayor
    ///   - VaR(95%) = el P&L en la posición 5 (el 5% peor)
    ///   - Si ese valor es -$12.50, el VaR es $12.50
    pub fn historical_var(&self, confidence: ConfidenceLevel) -> Option<f64> {
        if self.pnl_history.len() < 30 {
            // Mínimo 30 trades para que sea estadísticamente válido
            return None;
        }

        let mut pnls: Vec<f64> = self.pnl_history.iter()
            .map(|r| r.pnl)
            .collect();

        // Ordenar de menor (peor) a mayor (mejor)
        pnls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Encontrar el percentil de pérdida
        let alpha = confidence.alpha();
        let idx = (alpha * pnls.len() as f64).floor() as usize;
        let idx = idx.min(pnls.len() - 1);

        // VaR es la pérdida (valor negativo del P&L en ese percentil)
        // Lo expresamos como número positivo (pérdida)
        Some(-pnls[idx].min(0.0))
    }

    // ── VaR Paramétrico ──────────────────────────────────────

    /// VaR Paramétrico: asume distribución normal del P&L
    ///
    /// Fórmula: VaR = μ - z × σ
    /// Donde:
    ///   μ = media del P&L histórico
    ///   σ = desviación estándar del P&L histórico
    ///   z = z-score del nivel de confianza (1.645 para 95%)
    pub fn parametric_var(&self, confidence: ConfidenceLevel) -> Option<f64> {
        if self.pnl_history.len() < 30 {
            return None;
        }

        let pnls: Vec<f64> = self.pnl_history.iter().map(|r| r.pnl).collect();
        let n = pnls.len() as f64;

        // Media (μ)
        let mean = pnls.iter().sum::<f64>() / n;

        // Varianza y Desviación Estándar (σ)
        let variance = pnls.iter()
            .map(|&p| (p - mean).powi(2))
            .sum::<f64>() / (n - 1.0); // Varianza muestral (n-1)
        let std_dev = variance.sqrt();

        // VaR = -(μ - z × σ)
        // Si el resultado es negativo, no hay VaR (el percentil de pérdida es ganancia)
        let z = confidence.z_score();
        let var_value = -(mean - z * std_dev);

        Some(var_value.max(0.0))
    }

    // ── CVaR / Expected Shortfall ────────────────────────────

    /// CVaR (Conditional VaR) = Expected Shortfall
    ///
    /// Responde: "Dado que estoy en el peor X% de los casos,
    /// ¿cuál es la pérdida PROMEDIO?"
   ///
    /// CVaR es más informativo que VaR porque describe la cola,
    /// no solo el umbral.
    ///
    /// Ejemplo: VaR(95%) = $10 significa "no perderé más de $10 en el 95% de los casos"
   ///          CVaR(95%) = $18 significa "cuando sí pierdo más de $10, en promedio pierdo $18"
   pub fn cvar(&self, confidence: ConfidenceLevel) -> Option<f64> {
        if self.pnl_history.len() < 30 {
            return None;
        }

        let mut pnls: Vec<f64> = self.pnl_history.iter()
            .map(|r| r.pnl)
            .collect();

        pnls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let alpha = confidence.alpha();
        let cutoff_idx = (alpha * pnls.len() as f64).floor() as usize;
        let cutoff_idx = cutoff_idx.max(1).min(pnls.len() - 1);

        // Promedio de las pérdidas en la cola izquierda
        let tail_losses: Vec<f64> = pnls[..cutoff_idx].iter()
            .map(|&p| -p.min(0.0))
            .collect();

        if tail_losses.is_empty() {
            return Some(0.0);
        }

        let cvar = tail_losses.iter().sum::<f64>() / tail_losses.len() as f64;
        Some(cvar)
    }

    // ── VaR de Ejecución (específico para arbitraje) ─────────

    /// Calcula el VaR específico de una oportunidad de arbitraje
    /// considerando el riesgo de ejecución parcial (pierna incompleta)
    ///
    /// Este es el VaR más relevante para el bot de Polymarket.
    ///
    /// Escenarios modelados:
    ///   S1 (prob: fill_prob): Ambas piernas llenan → ganancia garantizada
    ///   S2 (prob: 1-fill_prob): Solo pierna 1 llena → exposición direccional
    ///   S3 (dentro de S2): El mercado resuelve en contra → pérdida máxima
    pub fn execution_var(
        &self,
        opportunity: &OpportunityRiskProfile,
        confidence: ConfidenceLevel,
    ) -> ExecutionVaRResult {
        let p_full = opportunity.fill_probability;
        let p_partial = 1.0 - p_full;

        // ── Escenario 1: Fill completo ────────────────────────
        // Resultado: ganancia garantizada (sin riesgo)
        let profit_full = opportunity.guaranteed_profit;

        // ── Escenario 2: Fill parcial (solo una pierna) ───────
        // Si compramos YES y NO no llena:
        //   - Si YES resuelve TRUE:  ganamos (1 - yes_price) × size
        //   - Si YES resuelve FALSE: perdemos yes_price × size
        //
        // Worst case: la pierna que llenó resuelve en contra
        let loss_if_partial = opportunity.worst_case_single_leg_loss;

        // ── Distribución de P&L ───────────────────────────────
        // Creamos una distribución discreta de outcomes:
        //
        //   P&L = +profit_full  con probabilidad p_full
        //   P&L = +small_gain   con probabilidad p_partial × 0.5  (pierna favorable)
        //   P&L = -loss_partial con probabilidad p_partial × 0.5  (pierna desfavorable)

        let small_gain = profit_full * 0.3; // Ganancia si la pierna que llenó resuelve bien

        // Simular distribución con muchos escenarios
        let n_scenarios = 10_000usize;
        let mut scenarios: Vec<f64> = Vec::with_capacity(n_scenarios);

        // Usar un generador simple pero reproducible basado en el estado
        let mut seed = 42u64;
        for _ in 0..n_scenarios {
            // LCG simple para números pseudo-aleatorios
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let rand = (seed >> 33) as f64 / u32::MAX as f64;

            let pnl = if rand < p_full {
                profit_full
            } else if rand < p_full + p_partial * 0.5 {
                small_gain
            } else {
                -loss_if_partial
            };
            scenarios.push(pnl);
        }

        scenarios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // VaR del escenario de ejecución
        let alpha = confidence.alpha();
        let var_idx = (alpha * n_scenarios as f64).floor() as usize;
        let var_idx = var_idx.min(n_scenarios - 1);
        let var_value = -scenarios[var_idx].min(0.0);

        // CVaR del escenario de ejecución
        let tail: Vec<f64> = scenarios[..var_idx.max(1)]
            .iter()
            .map(|&p| -p.min(0.0))
            .collect();
        let cvar_value = if tail.is_empty() {
            0.0
        } else {
            tail.iter().sum::<f64>() / tail.len() as f64
        };

        // Expected Value (EV) del trade
        let ev = scenarios.iter().sum::<f64>() / n_scenarios as f64;

        // Ratio Profit/VaR (cuánto ganas por cada dólar de riesgo VaR)
        // Mayor es mejor. < 1.0 = no vale la pena el riesgo
        let profit_var_ratio = if var_value > 0.0 {
            profit_full / var_value
        } else {
            f64::INFINITY // Sin riesgo VaR = ratio infinito
        };

        ExecutionVaRResult {
            var: var_value,
            cvar: cvar_value,
            expected_value: ev,
            profit_var_ratio,
            confidence_level: confidence,
            fill_probability: p_full,
            worst_case_loss: loss_if_partial,
            recommendation: determine_recommendation(
                var_value,
                profit_full,
                ev,
                self.daily_var_limit,
                self.daily_pnl,
                self.total_capital,
                p_full,
                loss_if_partial,
            ),
        }
    }

    // ── Límite Diario de VaR ──────────────────────────────────

    /// Verifica si ejecutar este trade violaría el límite diario de VaR
    ///
    /// Regla: el VaR acumulado del día no puede exceder daily_var_limit
    /// Esto previene días catastróficos donde múltiples trades fallan juntos
    pub fn check_daily_limit(&self, trade_var: f64) -> DailyLimitCheck {
        let remaining_budget = self.daily_var_limit + self.daily_pnl;

        // Regla 1: Ya perdimos más del límite hoy → bloquear todo
        if self.daily_pnl < -self.daily_var_limit {
            return DailyLimitCheck::Blocked {
                reason: format!(
                    "Límite diario alcanzado. P&L hoy: ${:.2}, límite: ${:.2}",
                    self.daily_pnl, -self.daily_var_limit
                ),
                daily_pnl: self.daily_pnl,
                daily_limit: self.daily_var_limit,
            };
        }

        // Regla 2: El VaR del trade excede el presupuesto restante → bloquear
        // Ejemplo: quedan $5 de presupuesto, el trade tiene VaR de $10 → bloqueado
        if trade_var >= remaining_budget {
            return DailyLimitCheck::Blocked {
                reason: format!(
                    "VaR del trade (${:.2}) excede presupuesto restante (${:.2})",
                    trade_var, remaining_budget
                ),
                daily_pnl: self.daily_pnl,
                daily_limit: self.daily_var_limit,
            };
        }

        // Regla 3: El trade consume más del 50% del presupuesto restante → advertir
        if trade_var > remaining_budget * 0.5 {
            return DailyLimitCheck::Warning {
                message: format!(
                    "Trade consume {:.1}% del presupuesto de riesgo restante",
                    (trade_var / remaining_budget) * 100.0
                ),
                trade_var,
                remaining_budget,
            };
        }

        DailyLimitCheck::Approved {
            trade_var,
            remaining_budget_after: remaining_budget - trade_var,
        }
    }

    // ── Estadísticas del Portafolio ───────────────────────────

    pub fn portfolio_stats(&self) -> PortfolioStats {
        let pnls: Vec<f64> = self.pnl_history.iter().map(|r| r.pnl).collect();

        if pnls.is_empty() {
            return PortfolioStats::default();
        }

        let n = pnls.len() as f64;
        let total_pnl: f64 = pnls.iter().sum();
        let mean_pnl = total_pnl / n;

        let wins: Vec<f64> = pnls.iter().filter(|&&p| p > 0.0).copied().collect();
        let losses: Vec<f64> = pnls.iter().filter(|&&p| p < 0.0).copied().collect();

        let win_rate = wins.len() as f64 / n;
        let avg_win = if wins.is_empty() { 0.0 } else { wins.iter().sum::<f64>() / wins.len() as f64 };
        let avg_loss = if losses.is_empty() { 0.0 } else { losses.iter().sum::<f64>() / losses.len() as f64 };

        // Profit Factor = suma de ganancias / suma de pérdidas absolutas
        let profit_factor = if losses.is_empty() || avg_loss == 0.0 {
            f64::INFINITY
        } else {
            let total_wins: f64 = wins.iter().sum();
            let total_losses: f64 = losses.iter().map(|l| l.abs()).sum();
            total_wins / total_losses
        };

        // Sharpe Ratio simplificado (usando media y std del P&L)
        let variance = pnls.iter().map(|&p| (p - mean_pnl).powi(2)).sum::<f64>() / (n - 1.0).max(1.0);
        let std_dev = variance.sqrt();
        let sharpe = if std_dev > 0.0 { mean_pnl / std_dev } else { 0.0 };

        // Maximum Drawdown
        let max_drawdown = calculate_max_drawdown(&pnls);

        PortfolioStats {
            total_trades: pnls.len(),
            total_pnl,
            daily_pnl: self.daily_pnl,
            win_rate,
            avg_win,
            avg_loss,
            profit_factor,
            sharpe_ratio: sharpe,
            max_drawdown,
            var_95: self.historical_var(ConfidenceLevel::Pct95).unwrap_or(0.0),
            cvar_95: self.cvar(ConfidenceLevel::Pct95).unwrap_or(0.0),
            var_99: self.historical_var(ConfidenceLevel::Pct99).unwrap_or(0.0),
        }
    }
}

// ── Tipos de Soporte ─────────────────────────────────────────

/// Perfil de riesgo de una oportunidad de arbitraje
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OpportunityRiskProfile {
    pub opportunity_id: String,
    /// Ganancia garantizada si ambas piernas llenan
    pub guaranteed_profit: f64,
    /// Probabilidad de que TODAS las órdenes llenen
    pub fill_probability: f64,
    /// Pérdida máxima si solo llena una pierna y resuelve en contra
    pub worst_case_single_leg_loss: f64,
    /// Capital total comprometido
    pub capital_committed: f64,
}

impl OpportunityRiskProfile {
    /// Construye el perfil desde una oportunidad de arbitraje
    pub fn from_opportunity(
        opp_id: &str,
        guaranteed_profit: f64,
        yes_price: f64,
        no_price: f64,
        position_size: f64,
        yes_volume: f64,
        no_volume: f64,
    ) -> Self {
        // Pérdida worst-case: si compras YES a yes_price y resuelve FALSE
        // (o compras NO a no_price y resuelve TRUE)
        let worst_leg_price = yes_price.max(no_price);
        let worst_case_loss = worst_leg_price * position_size;

        // Estimar probabilidad de fill basada en liquidez disponible
        let min_liquidity = yes_volume.min(no_volume);
        let fill_probability = if min_liquidity <= 0.0 {
            0.0
        } else {
            // Función sigmoide: mucha liquidez → alta prob de fill
            let ratio = min_liquidity / position_size;
            1.0 - (-2.0 * ratio).exp()
        };

        OpportunityRiskProfile {
            opportunity_id: opp_id.to_string(),
            guaranteed_profit,
            fill_probability,
            worst_case_single_leg_loss: worst_case_loss,
            capital_committed: (yes_price + no_price) * position_size,
        }
    }
}

/// Resultado del cálculo de VaR de ejecución
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionVaRResult {
    /// VaR al nivel de confianza configurado (en $)
    pub var: f64,
    /// CVaR / Expected Shortfall (en $)
    pub cvar: f64,
    /// Expected Value del trade (en $)
    pub expected_value: f64,
    /// Ratio ganancia / VaR (mayor = mejor)
    pub profit_var_ratio: f64,
    pub confidence_level: ConfidenceLevel,
    pub fill_probability: f64,
    pub worst_case_loss: f64,
    /// Recomendación del sistema de riesgo
    pub recommendation: RiskRecommendation,
}

/// Recomendación del motor de riesgo
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RiskRecommendation {
    /// Ejecutar el trade
    Execute,
    /// Ejecutar con posición reducida
    ExecuteReduced { suggested_size_pct: f64 },
    /// No ejecutar — riesgo excesivo
    Skip { reason: String },
}

/// Verificación del límite diario
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DailyLimitCheck {
    Approved { trade_var: f64, remaining_budget_after: f64 },
    Warning { message: String, trade_var: f64, remaining_budget: f64 },
    Blocked { reason: String, daily_pnl: f64, daily_limit: f64 },
}

impl DailyLimitCheck {
    pub fn is_blocked(&self) -> bool {
        matches!(self, DailyLimitCheck::Blocked { .. })
    }
}

/// Estadísticas completas del portafolio
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PortfolioStats {
    pub total_trades: usize,
    pub total_pnl: f64,
    pub daily_pnl: f64,
    pub win_rate: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub profit_factor: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown: f64,
    pub var_95: f64,
    pub cvar_95: f64,
    pub var_99: f64,
}

// ── Funciones auxiliares ─────────────────────────────────────

/// Determina la recomendación de ejecución basada en el análisis de riesgo
fn determine_recommendation(
    var: f64,
    profit: f64,
    ev: f64,
    daily_var_limit: f64,
    daily_pnl: f64,
    total_capital: f64,
    fill_probability: f64,
    worst_case_loss: f64,
) -> RiskRecommendation {
    // Regla 1: EV negativo → nunca ejecutar
    if ev <= 0.0 {
        return RiskRecommendation::Skip {
            reason: format!("Expected Value negativo: ${:.4}", ev),
        };
    }

    // Regla 2: Fill probability muy baja con pérdida alta → Skip
    // Ejemplo: fill_prob=0.40, worst_case=$0.60, profit=$0.05
    // Riesgo esperado de pierna = (1-0.40) * 0.50 * 0.60 = $0.18 >> profit=$0.05
    let expected_partial_loss = (1.0 - fill_probability) * 0.5 * worst_case_loss;
    if expected_partial_loss > profit * 2.0 {
        return RiskRecommendation::Skip {
            reason: format!(
                "Pérdida esperada por fill parcial (${:.4}) excede 2x ganancia (${:.4}). Fill prob: {:.0}%",
                expected_partial_loss, profit, fill_probability * 100.0
            ),
        };
    }

    // Regla 3: VaR excede 3x la ganancia garantizada → ratio riesgo/beneficio negativo
    if var > profit * 3.0 {
        return RiskRecommendation::Skip {
            reason: format!(
                "VaR (${:.4}) excede 3x la ganancia (${:.4})",
                var, profit
            ),
        };
    }

    // Regla 4: Ya perdimos demasiado hoy (80% del límite diario)
    if daily_pnl < -daily_var_limit * 0.8 {
        return RiskRecommendation::Skip {
            reason: format!(
                "Límite diario al 80%: P&L hoy = ${:.2}",
                daily_pnl
            ),
        };
    }

    // Regla 5: VaR excede el 2% del capital total → reducir posición
    let var_pct_of_capital = var / total_capital;
    if var_pct_of_capital > 0.02 {
        let suggested_size = 0.02 / var_pct_of_capital;
        return RiskRecommendation::ExecuteReduced {
            suggested_size_pct: suggested_size,
        };
    }

    RiskRecommendation::Execute
}

/// Calcula el Maximum Drawdown de una serie de P&L
/// Drawdown = caída desde el pico más alto hasta el punto más bajo
fn calculate_max_drawdown(pnls: &[f64]) -> f64 {
    if pnls.is_empty() { return 0.0; }

    let mut peak = 0.0f64;
    let mut cumulative = 0.0f64;
    let mut max_dd = 0.0f64;

    for &pnl in pnls {
        cumulative += pnl;
        peak = peak.max(cumulative);
        let drawdown = peak - cumulative;
        max_dd = max_dd.max(drawdown);
    }

    max_dd
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine_with_history(pnls: Vec<f64>) -> VaREngine {
        let mut engine = VaREngine::new(ConfidenceLevel::Pct95, 100.0, 1000.0);
        for (i, pnl) in pnls.into_iter().enumerate() {
            engine.record_trade(PnLRecord {
                trade_id: format!("trade_{}", i),
               pnl,
                capital_at_risk: 50.0,
                execution_type: ExecutionType::FullFill,
                timestamp: Utc::now(),
            });
        }
        engine
    }

    #[test]
    fn test_historical_var_needs_minimum_history() {
        let engine = make_engine_with_history(vec![1.0, 2.0, -1.0]);
        // Menos de 30 trades → None
        assert!(engine.historical_var(ConfidenceLevel::Pct95).is_none());
    }

    #[test]
    fn test_historical_var_with_sufficient_history() {
        // 100 trades: 90 ganan $1, 10 pierden $10
        let mut pnls: Vec<f64> = (0..90).map(|_| 1.0).collect();
        pnls.extend((0..10).map(|_| -10.0));

        let engine = make_engine_with_history(pnls);
        let var_95 = engine.historical_var(ConfidenceLevel::Pct95).unwrap();

        // VaR(95%) debe ser ~$10 (el 5% peor son las pérdidas de $10)
        assert!(var_95 > 0.0, "VaR debe ser positivo");
       println!("VaR histórico 95%: ${:.2}", var_95);
   }

    #[test]
    fn test_parametric_var() {
        let pnls: Vec<f64> = (0..100).map(|i| if i < 95 { 1.0 } else { -5.0 }).collect();
        let engine = make_engine_with_history(pnls);

        let var_param = engine.parametric_var(ConfidenceLevel::Pct95).unwrap();
        println!("VaR paramétrico 95%: ${:.2}", var_param);
       assert!(var_param >= 0.0);
    }

    #[test]
    fn test_cvar_greater_than_var() {
        let mut pnls: Vec<f64> = (0..90).map(|_| 1.0).collect();
        pnls.extend([-5.0, -8.0, -12.0, -15.0, -20.0,
                     -6.0, -9.0, -11.0, -14.0, -18.0]);

        let engine = make_engine_with_history(pnls);
        let var_95 = engine.historical_var(ConfidenceLevel::Pct95).unwrap();
        let cvar_95 = engine.cvar(ConfidenceLevel::Pct95).unwrap();

        // CVaR siempre debe ser >= VaR (es el promedio de la cola, no el umbral)
        println!("VaR 95%: ${:.2}, CVaR 95%: ${:.2}", var_95, cvar_95);
       assert!(cvar_95 >= var_95 - 0.01, "CVaR debe ser >= VaR");
   }

    #[test]
    fn test_execution_var_high_fill_probability() {
        let engine = VaREngine::new(ConfidenceLevel::Pct95, 100.0, 1000.0);

        let profile = OpportunityRiskProfile {
            opportunity_id: "test_opp".to_string(),
           guaranteed_profit: 0.10,
            fill_probability: 0.95, // Alta probabilidad de fill completo
            worst_case_single_leg_loss: 0.40,
            capital_committed: 0.85,
        };

        let result = engine.execution_var(&profile, ConfidenceLevel::Pct95);
        println!("Execution VaR: ${:.4}, CVaR: ${:.4}, EV: ${:.4}", result.var, result.cvar, result.expected_value);
       println!("Profit/VaR ratio: {:.2}", result.profit_var_ratio);
       println!("Recomendación: {:?}", result.recommendation);

       // Con 95% de fill y ganancia garantizada, EV debe ser positivo
        assert!(result.expected_value > 0.0);
    }

    #[test]
    fn test_execution_var_low_fill_probability() {
        let engine = VaREngine::new(ConfidenceLevel::Pct95, 100.0, 1000.0);

        let profile = OpportunityRiskProfile {
            opportunity_id: "test_risky".to_string(),
           guaranteed_profit: 0.05,
            fill_probability: 0.40, // Baja liquidez
            worst_case_single_leg_loss: 0.60,
            capital_committed: 1.0,
        };

        let result = engine.execution_var(&profile, ConfidenceLevel::Pct95);
        println!("Risky trade - Recomendación: {:?}", result.recommendation);

       // Con bajo fill y pérdida alta, debería recomendar Skip
        assert!(matches!(result.recommendation, RiskRecommendation::Skip { .. }));
    }

    #[test]
    fn test_daily_limit_check() {
        let mut engine = VaREngine::new(ConfidenceLevel::Pct95, 50.0, 1000.0);

        // Simular pérdidas del día
        engine.daily_pnl = -45.0; // Ya perdimos $45 de $50 límite

        let check = engine.check_daily_limit(10.0); // Intentar trade con VaR $10
        println!("Límite diario: {:?}", check);

       // Debería bloquear (ya estamos al 90% del límite)
        assert!(check.is_blocked());
    }

    #[test]
    fn test_max_drawdown() {
        // P&L: gana $10, pierde $15, gana $5, pierde $8
        // Equity curve: 0 → 10 → -5 → 0 → -8
        // Drawdown desde pico $10 hasta $-8 = $18
        let pnls = vec![10.0, -15.0, 5.0, -8.0];
        let dd = calculate_max_drawdown(&pnls);
        println!("Max Drawdown: ${:.2}", dd);
       assert!((dd - 18.0).abs() < 0.01);
    }

    #[test]
    fn test_portfolio_stats() {
        let mut pnls: Vec<f64> = (0..70).map(|_| 0.08).collect();
        pnls.extend((0..30).map(|_| -0.40));

        let engine = make_engine_with_history(pnls);
        let stats = engine.portfolio_stats();

        println!("Win rate: {:.1}%", stats.win_rate * 100.0);
       println!("Profit factor: {:.2}", stats.profit_factor);
       println!("Sharpe ratio: {:.2}", stats.sharpe_ratio);
       println!("Max drawdown: ${:.2}", stats.max_drawdown);
       println!("VaR 95%: ${:.4}", stats.var_95);
       println!("CVaR 95%: ${:.4}", stats.cvar_95);

        assert_eq!(stats.total_trades, 100);
        assert!((stats.win_rate - 0.70).abs() < 0.01);
    }
}