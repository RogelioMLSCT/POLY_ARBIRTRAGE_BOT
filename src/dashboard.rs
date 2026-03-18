// ============================================================
// dashboard.rs — Servidor HTTP para el dashboard de monitoreo
// ============================================================
//
// Expone una API JSON en http://localhost:3000
// El frontend HTML se sirve desde /static/dashboard.html
//
// Endpoints:
//   GET /api/stats          → Estadísticas del bot
//   GET /api/markets        → Lista de mercados monitoreados
//   GET /api/opportunities  → Oportunidades detectadas
//   GET /api/trades         → Historial de trades
//   GET /health             → Health check
// ============================================================

use std::sync::Arc;
use anyhow::Result;
use axum::{
    extract::State,
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Serialize;
use tracing::info;

use crate::types::AppState;

// ── Servidor ─────────────────────────────────────────────────

pub async fn run(state: Arc<AppState>) -> Result<()> {
    let port = state.config.dashboard_port;
    let addr = format!("0.0.0.0:{}", port);

   info!("Dashboard disponible en http://localhost:{}", port);

   // Definir rutas
    let app = Router::new()
        .route("/", get(serve_dashboard))
       .route("/api/stats", get(get_stats))
       .route("/api/markets", get(get_markets))
       .route("/api/opportunities", get(get_opportunities))
       .route("/api/trades", get(get_trades))
       .route("/api/risk", get(get_risk_metrics))
       .route("/health", get(health_check))
       .with_state(state);

    // Iniciar servidor
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Handlers ─────────────────────────────────────────────────

/// Sirve el HTML del dashboard
async fn serve_dashboard() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

/// Health check
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "timestamp": chrono::Utc::now() }))
}

/// Estadísticas del bot
async fn get_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
   let stats = state.stats.read().await;
    Json(ApiStats {
        opportunities_detected: stats.opportunities_detected,
        opportunities_executed: stats.opportunities_executed,
        opportunities_failed: stats.opportunities_failed,
        total_profit_usd: stats.total_profit_usd,
        markets_monitored: state.markets.len(),
        active_opportunities: state.opportunities.len(),
        last_opportunity_at: stats.last_opportunity_at.map(|t| t.to_rfc3339()),
    })
}

/// Lista de mercados con precios actuales
async fn get_markets(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut markets: Vec<MarketSummary> = state.markets.iter()
        .map(|entry| {
            let m = entry.value();
            MarketSummary {
                condition_id: m.condition_id[..12.min(m.condition_id.len())].to_string(),
                question: m.question[..60.min(m.question.len())].to_string(),
                yes_price: m.yes_price,
                no_price: m.no_price,
                price_sum: m.yes_price + m.no_price,
                deviation: ((m.yes_price + m.no_price) - 1.0).abs(),
                last_updated: m.last_updated.to_rfc3339(),
            }
        })
        .collect();

    // Ordenar por mayor desviación (más arbitraje posible primero)
    markets.sort_by(|a, b| b.deviation.partial_cmp(&a.deviation).unwrap_or(std::cmp::Ordering::Equal));
    markets.truncate(50); // Máximo 50 mercados en la respuesta

    Json(markets)
}

/// Oportunidades de arbitraje detectadas
async fn get_opportunities(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut opps: Vec<OpportunitySummary> = state.opportunities.iter()
        .map(|entry| {
            let o = entry.value();
            OpportunitySummary {
                id: o.id[..8].to_string(),
                opportunity_type: format!("{:?}", o.opportunity_type),
               guaranteed_profit: o.guaranteed_profit,
                max_profit: o.max_profit,
                position_size: o.position_size,
                status: format!("{:?}", o.status),
               detected_at: o.detected_at.to_rfc3339(),
                profit_pct: if o.position_size > 0.0 {
                    (o.guaranteed_profit / o.position_size) * 100.0
                } else { 0.0 },
            }
        })
        .collect();

    opps.sort_by(|a, b| b.guaranteed_profit.partial_cmp(&a.guaranteed_profit).unwrap_or(std::cmp::Ordering::Equal));
    opps.truncate(20);

    Json(opps)
}

/// Historial de trades ejecutados
async fn get_trades(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut trades: Vec<TradeSummary> = state.executed_trades.iter()
        .map(|entry| {
            let t = entry.value();
            TradeSummary {
                id: t.opportunity_id[..8].to_string(),
                actual_profit: t.actual_profit,
                success: t.success,
                orders_count: t.orders_filled.len(),
                executed_at: t.executed_at.to_rfc3339(),
                error: t.error.clone(),
            }
        })
        .collect();

    trades.sort_by(|a, b| b.executed_at.cmp(&a.executed_at));
    trades.truncate(50);

    Json(trades)
}

/// Métricas de riesgo VaR en tiempo real
async fn get_risk_metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let var_engine = state.var_engine.read().await;
    let stats = var_engine.portfolio_stats();

    Json(RiskMetrics {
        daily_pnl: stats.daily_pnl,
        daily_var_limit: state.config.daily_var_limit,
        daily_limit_used_pct: (-stats.daily_pnl / state.config.daily_var_limit * 100.0).max(0.0),
        var_95: stats.var_95,
        cvar_95: stats.cvar_95,
        var_99: stats.var_99,
        win_rate: stats.win_rate,
        profit_factor: stats.profit_factor,
        sharpe_ratio: stats.sharpe_ratio,
        max_drawdown: stats.max_drawdown,
        total_trades: stats.total_trades,
    })
}

// ── Tipos de Respuesta JSON ──────────────────────────────────

#[derive(Serialize)]
struct ApiStats {
    opportunities_detected: u64,
    opportunities_executed: u64,
    opportunities_failed: u64,
    total_profit_usd: f64,
    markets_monitored: usize,
    active_opportunities: usize,
    last_opportunity_at: Option<String>,
}

#[derive(Serialize)]
struct MarketSummary {
    condition_id: String,
    question: String,
    yes_price: f64,
    no_price: f64,
    price_sum: f64,
    deviation: f64,
    last_updated: String,
}

#[derive(Serialize)]
struct OpportunitySummary {
    id: String,
    opportunity_type: String,
    guaranteed_profit: f64,
    max_profit: f64,
    position_size: f64,
    profit_pct: f64,
    status: String,
    detected_at: String,
}

#[derive(Serialize)]
struct TradeSummary {
    id: String,
    actual_profit: f64,
    success: bool,
    orders_count: usize,
    executed_at: String,
    error: Option<String>,
}

#[derive(Serialize)]
struct RiskMetrics {
    daily_pnl: f64,
    daily_var_limit: f64,
    daily_limit_used_pct: f64,
    var_95: f64,
    cvar_95: f64,
    var_99: f64,
    win_rate: f64,
    profit_factor: f64,
    sharpe_ratio: f64,
    max_drawdown: f64,
    total_trades: usize,
}

// ── Dashboard HTML (embebido en el binario) ──────────────────

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="es">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Polymarket Arbitrage Bot</title>
<style>
 @import url('https://fonts.googleapis.com/css2?family=Space+Mono:wght@400;700&family=DM+Sans:wght@300;400;600&display=swap');

  :root {
    --bg: #0a0a0f;
    --surface: #111118;
    --surface2: #1a1a24;
    --border: #2a2a3a;
    --accent: #00ff88;
    --accent2: #0088ff;
    --danger: #ff4455;
    --warning: #ffaa00;
    --text: #e0e0f0;
    --muted: #606080;
  }

  * { box-sizing: border-box; margin: 0; padding: 0; }

  body {
    background: var(--bg);
    color: var(--text);
    font-family: 'DM Sans', sans-serif;
    min-height: 100vh;
    overflow-x: hidden;
  }

  /* Grid de fondo */
  body::before {
    content: '';
    position: fixed;
    inset: 0;
    background-image:
      linear-gradient(rgba(0,255,136,0.03) 1px, transparent 1px),
      linear-gradient(90deg, rgba(0,255,136,0.03) 1px, transparent 1px);
    background-size: 40px 40px;
    pointer-events: none;
    z-index: 0;
  }

  .container { max-width: 1400px; margin: 0 auto; padding: 24px; position: relative; z-index: 1; }

  /* Header */
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 32px;
    padding-bottom: 20px;
    border-bottom: 1px solid var(--border);
  }

  .logo {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .logo-icon {
    width: 40px; height: 40px;
    background: linear-gradient(135deg, var(--accent), var(--accent2));
    border-radius: 10px;
    display: flex; align-items: center; justify-content: center;
    font-size: 20px;
  }

  h1 {
    font-family: 'Space Mono', monospace;
    font-size: 1.2rem;
    letter-spacing: -0.02em;
    color: var(--text);
  }

  h1 span { color: var(--accent); }

  .status-indicator {
    display: flex;
    align-items: center;
    gap: 8px;
    font-family: 'Space Mono', monospace;
    font-size: 0.75rem;
    color: var(--accent);
  }

  .dot {
    width: 8px; height: 8px;
    background: var(--accent);
    border-radius: 50%;
    animation: pulse 2s infinite;
  }

  @keyframes pulse {
    0%, 100% { opacity: 1; transform: scale(1); }
    50% { opacity: 0.5; transform: scale(0.8); }
  }

  /* Stats Grid */
  .stats-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 16px;
    margin-bottom: 32px;
  }

  .stat-card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 20px;
    position: relative;
    overflow: hidden;
    transition: border-color 0.2s;
  }

  .stat-card:hover { border-color: var(--accent); }

  .stat-card::before {
    content: '';
    position: absolute;
    top: 0; left: 0; right: 0;
    height: 2px;
    background: linear-gradient(90deg, var(--accent), transparent);
  }

  .stat-label {
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--muted);
    margin-bottom: 8px;
  }

  .stat-value {
    font-family: 'Space Mono', monospace;
    font-size: 1.6rem;
    font-weight: 700;
    color: var(--text);
  }

  .stat-value.positive { color: var(--accent); }
  .stat-value.negative { color: var(--danger); }
  .stat-value.warning { color: var(--warning); }

  /* Main Grid */
  .main-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 24px;
    margin-bottom: 24px;
  }

  @media (max-width: 900px) { .main-grid { grid-template-columns: 1fr; } }

  .panel {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
    overflow: hidden;
  }

  .panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px;
    border-bottom: 1px solid var(--border);
    background: var(--surface2);
  }

  .panel-title {
    font-family: 'Space Mono', monospace;
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--muted);
  }

  .badge {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 20px;
    padding: 2px 10px;
    font-size: 0.7rem;
    font-family: 'Space Mono', monospace;
    color: var(--accent);
  }

  .panel-body { padding: 4px 0; max-height: 360px; overflow-y: auto; }

  /* Scrollbar */
  .panel-body::-webkit-scrollbar { width: 4px; }
  .panel-body::-webkit-scrollbar-track { background: transparent; }
  .panel-body::-webkit-scrollbar-thumb { background: var(--border); border-radius: 2px; }

  /* Table rows */
  .row {
    display: grid;
    padding: 12px 20px;
    border-bottom: 1px solid rgba(255,255,255,0.04);
    align-items: center;
    transition: background 0.15s;
    font-size: 0.85rem;
  }

  .row:hover { background: rgba(0,255,136,0.04); }
  .row:last-child { border-bottom: none; }

  .market-row { grid-template-columns: 1fr 80px 80px 80px; gap: 8px; }
  .opp-row { grid-template-columns: 80px 1fr 90px 80px; gap: 8px; }
  .trade-row { grid-template-columns: 80px 90px 90px 1fr; gap: 8px; }

  .label-header {
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--muted);
    padding: 8px 20px 4px;
    display: grid;
    border-bottom: 1px solid var(--border);
  }

  .label-header.market-row,
  .label-header.opp-row,
  .label-header.trade-row { background: transparent; }

  .price-good { color: var(--text); }
  .price-bad { color: var(--danger); font-weight: 600; }
  .price-warn { color: var(--warning); }

  .profit-badge {
    display: inline-block;
    background: rgba(0,255,136,0.1);
    border: 1px solid rgba(0,255,136,0.3);
    color: var(--accent);
    border-radius: 6px;
    padding: 2px 8px;
    font-family: 'Space Mono', monospace;
    font-size: 0.75rem;
  }

  .status-badge {
    display: inline-block;
    border-radius: 4px;
    padding: 2px 7px;
    font-size: 0.68rem;
    font-family: 'Space Mono', monospace;
    text-transform: uppercase;
  }
  .s-detected { background: rgba(0,136,255,0.15); color: var(--accent2); }
  .s-executed { background: rgba(0,255,136,0.15); color: var(--accent); }
  .s-failed   { background: rgba(255,68,85,0.15);  color: var(--danger); }
  .s-expired  { background: rgba(100,100,120,0.15); color: var(--muted); }

  .truncate { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

  .mono { font-family: 'Space Mono', monospace; }

  /* Dry run banner */
  .dry-run-banner {
    background: rgba(255,170,0,0.1);
    border: 1px solid rgba(255,170,0,0.3);
    border-radius: 8px;
    padding: 12px 20px;
    margin-bottom: 24px;
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 0.85rem;
    color: var(--warning);
  }

  /* Refresh timer */
  .refresh-bar {
    height: 2px;
    background: var(--border);
    border-radius: 1px;
    margin-bottom: 32px;
    overflow: hidden;
  }

  .refresh-progress {
    height: 100%;
    background: linear-gradient(90deg, var(--accent), var(--accent2));
    animation: progress 5s linear infinite;
    transform-origin: left;
  }

  @keyframes progress {
    0% { transform: scaleX(0); }
    100% { transform: scaleX(1); }
  }

  .empty-state {
    padding: 40px 20px;
    text-align: center;
    color: var(--muted);
    font-size: 0.85rem;
  }
</style>
</head>
<body>
<div class="container">
 <header>
    <div class="logo">
     <div class="logo-icon"></div>
     <div>
        <h1>Poly<span>Arb</span> Bot</h1>
        <div style="font-size:0.7rem;color:var(--muted);margin-top:2px">Polymarket Arbitrage System</div>
     </div>
    </div>
    <div class="status-indicator">
     <div class="dot"></div>
     LIVE
    </div>
  </header>

  <div class="refresh-bar"><div class="refresh-progress"></div></div>

 <div id="dry-run-banner" class="dry-run-banner" style="display:none">
   ️ <strong>MODO SIMULACIÓN</strong> — Los trades NO son reales. Configura <code>DRY_RUN=false</code> para trading real.
  </div>

  <!-- Stats -->
  <div class="stats-grid" id="stats-grid">
   <div class="stat-card"><div class="stat-label">Profit Total</div><div class="stat-value positive" id="stat-profit">$0.00</div></div>
   <div class="stat-card"><div class="stat-label">Trades Ejecutados</div><div class="stat-value" id="stat-executed">0</div></div>
   <div class="stat-card"><div class="stat-label">Oportunidades</div><div class="stat-value warning" id="stat-opps">0</div></div>
   <div class="stat-card"><div class="stat-label">Mercados</div><div class="stat-value" id="stat-markets">0</div></div>
   <div class="stat-card"><div class="stat-label">Fallidos</div><div class="stat-value negative" id="stat-failed">0</div></div>
   <div class="stat-card"><div class="stat-label">Tasa Éxito</div><div class="stat-value positive" id="stat-rate">—</div></div>
 </div>

  <!-- Main Panels -->
  <div class="main-grid">
   <!-- Oportunidades -->
    <div class="panel">
     <div class="panel-header">
       <span class="panel-title">Oportunidades Activas</span>
       <span class="badge" id="opp-count">0</span>
     </div>
      <div class="label-header opp-row">
       <span>ID</span><span>Tipo</span><span>Profit $</span><span>Estado</span>
      </div>
      <div class="panel-body" id="opportunities-list">
       <div class="empty-state">Escaneando mercados...</div>
     </div>
    </div>

    <!-- Panel de VaR -->
    <div class="panel">
     <div class="panel-header">
       <span class="panel-title">Risk Monitor (VaR)</span>
       <span class="badge" id="var-status-badge">—</span>
     </div>
      <div class="panel-body" style="padding: 20px;">

       <!-- Límite diario -->
        <div style="margin-bottom:20px">
         <div style="display:flex;justify-content:space-between;margin-bottom:6px">
           <span style="font-size:0.72rem;text-transform:uppercase;letter-spacing:.08em;color:var(--muted)">Límite Diario Usado</span>
           <span class="mono" id="var-daily-pct" style="font-size:0.8rem">0%</span>
         </div>
          <div style="height:8px;background:var(--surface2);border-radius:4px;overflow:hidden">
           <div id="var-daily-bar" style="height:100%;border-radius:4px;background:linear-gradient(90deg,var(--accent),var(--accent2));width:0%;transition:width .5s ease"></div>
         </div>
          <div style="display:flex;justify-content:space-between;margin-top:4px">
           <span style="font-size:0.68rem;color:var(--muted)">P&L hoy: <span id="var-daily-pnl" class="mono">$0.00</span></span>
           <span style="font-size:0.68rem;color:var(--muted)">Límite: <span id="var-daily-limit" class="mono">—</span></span>
         </div>
        </div>

        <!-- Métricas VaR -->
        <div style="display:grid;grid-template-columns:1fr 1fr;gap:12px;margin-bottom:16px">
         <div style="background:var(--surface2);border:1px solid var(--border);border-radius:8px;padding:12px">
           <div style="font-size:0.65rem;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin-bottom:4px">VaR 95%</div>
           <div class="mono" id="var-95" style="font-size:1.1rem;color:var(--warning)">—</div>
         </div>
          <div style="background:var(--surface2);border:1px solid var(--border);border-radius:8px;padding:12px">
           <div style="font-size:0.65rem;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin-bottom:4px">CVaR 95%</div>
           <div class="mono" id="cvar-95" style="font-size:1.1rem;color:var(--danger)">—</div>
         </div>
          <div style="background:var(--surface2);border:1px solid var(--border);border-radius:8px;padding:12px">
           <div style="font-size:0.65rem;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin-bottom:4px">VaR 99%</div>
           <div class="mono" id="var-99" style="font-size:1.1rem;color:var(--danger)">—</div>
         </div>
          <div style="background:var(--surface2);border:1px solid var(--border);border-radius:8px;padding:12px">
           <div style="font-size:0.65rem;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin-bottom:4px">Max Drawdown</div>
           <div class="mono" id="max-dd" style="font-size:1.1rem;color:var(--danger)">—</div>
         </div>
        </div>

        <!-- Métricas de performance -->
        <div style="display:grid;grid-template-columns:repeat(3,1fr);gap:8px">
         <div style="text-align:center">
           <div style="font-size:0.65rem;color:var(--muted);margin-bottom:2px">Win Rate</div>
           <div class="mono" id="win-rate" style="font-size:0.9rem;color:var(--accent)">—</div>
         </div>
          <div style="text-align:center">
           <div style="font-size:0.65rem;color:var(--muted);margin-bottom:2px">Profit Factor</div>
           <div class="mono" id="profit-factor" style="font-size:0.9rem">—</div>
         </div>
          <div style="text-align:center">
           <div style="font-size:0.65rem;color:var(--muted);margin-bottom:2px">Sharpe</div>
           <div class="mono" id="sharpe" style="font-size:0.9rem">—</div>
         </div>
        </div>

        <div style="margin-top:12px;padding-top:12px;border-top:1px solid var(--border);font-size:0.7rem;color:var(--muted);text-align:center">
         Basado en <span id="var-trades">0</span> trades históricos
       </div>
      </div>
    </div>
  </div>

  <!-- Mercados con mayor desviación -->
  <div class="panel" style="margin-bottom:24px">
   <div class="panel-header">
     <span class="panel-title">Mercados (por desviación)</span>
     <span class="badge" id="market-count">0</span>
   </div>
    <div class="label-header market-row">
     <span>Pregunta</span><span>YES</span><span>NO</span><span>Suma</span>
    </div>
    <div class="panel-body" id="markets-list">
     <div class="empty-state">Conectando feed...</div>
   </div>
  </div>

  <!-- Trades recientes -->
  <div class="panel">
   <div class="panel-header">
     <span class="panel-title">Trades Recientes</span>
     <span class="badge" id="trade-count">0</span>
   </div>
    <div class="label-header trade-row">
     <span>ID</span><span>Profit</span><span>Estado</span><span>Timestamp</span>
    </div>
    <div class="panel-body" id="trades-list">
     <div class="empty-state">Sin trades aún.</div>
   </div>
  </div>
</div>

<script>
const fmt = {
  usd: v => v >= 0 ? `+$${v.toFixed(4)}` : `-$${Math.abs(v).toFixed(4)}`,
  pct: v => `${v.toFixed(2)}%`,
  price: v => v.toFixed(3),
};

function statusClass(s) {
  const m = { Detected:'s-detected', Executed:'s-executed', Failed:'s-failed', Expired:'s-expired', Executing:'s-detected' };
  return m[s] || 's-expired';
}

async function fetchAndUpdate() {
  try {
    const [stats, markets, opps, trades, risk] = await Promise.all([
      fetch('/api/stats').then(r=>r.json()),
      fetch('/api/markets').then(r=>r.json()),
      fetch('/api/opportunities').then(r=>r.json()),
      fetch('/api/trades').then(r=>r.json()),
      fetch('/api/risk').then(r=>r.json()),
    ]);

    // Stats
    document.getElementById('stat-profit').textContent = `$${stats.total_profit_usd.toFixed(2)}`;
    document.getElementById('stat-executed').textContent = stats.opportunities_executed;
    document.getElementById('stat-opps').textContent = stats.active_opportunities;
    document.getElementById('stat-markets').textContent = stats.markets_monitored;
    document.getElementById('stat-failed').textContent = stats.opportunities_failed;
    const total = stats.opportunities_executed + stats.opportunities_failed;
    document.getElementById('stat-rate').textContent = total > 0
      ? `${((stats.opportunities_executed/total)*100).toFixed(1)}%` : '—';

    // ── VaR Panel ──────────────────────────────────────────
    const usedPct = Math.min(risk.daily_limit_used_pct, 100);
    document.getElementById('var-daily-pct').textContent = `${usedPct.toFixed(1)}%`;
    document.getElementById('var-daily-pnl').textContent = `$${risk.daily_pnl.toFixed(2)}`;
    document.getElementById('var-daily-limit').textContent = `$${risk.daily_var_limit.toFixed(0)}`;

    const bar = document.getElementById('var-daily-bar');
    bar.style.width = `${usedPct}%`;
    bar.style.background = usedPct > 80
      ? 'linear-gradient(90deg,var(--danger),#ff8800)'
      : usedPct > 50
        ? 'linear-gradient(90deg,var(--warning),var(--accent))'
        : 'linear-gradient(90deg,var(--accent),var(--accent2))';

    const badge = document.getElementById('var-status-badge');
    if (usedPct >= 100) {
      badge.textContent = 'BLOQUEADO'; badge.style.color = 'var(--danger)';
    } else if (usedPct >= 80) {
      badge.textContent = 'ALERTA'; badge.style.color = 'var(--warning)';
    } else {
      badge.textContent = 'OK'; badge.style.color = 'var(--accent)';
    }

    // Métricas VaR (solo si hay suficiente historial)
    const fmtVar = v => v > 0 ? `$${v.toFixed(4)}` : '<span style="color:var(--muted)">sin datos</span>';
   document.getElementById('var-95').innerHTML = fmtVar(risk.var_95);
    document.getElementById('cvar-95').innerHTML = fmtVar(risk.cvar_95);
    document.getElementById('var-99').innerHTML = fmtVar(risk.var_99);
    document.getElementById('max-dd').innerHTML = risk.max_drawdown > 0
      ? `$${risk.max_drawdown.toFixed(4)}` : '<span style="color:var(--muted)">—</span>';

   document.getElementById('win-rate').textContent = risk.total_trades >= 30
      ? `${(risk.win_rate*100).toFixed(1)}%` : '—';
    document.getElementById('profit-factor').textContent = risk.total_trades >= 30
      ? (risk.profit_factor === Infinity ? '∞' : risk.profit_factor.toFixed(2)) : '—';
    document.getElementById('sharpe').textContent = risk.total_trades >= 30
      ? risk.sharpe_ratio.toFixed(2) : '—';
    document.getElementById('var-trades').textContent = risk.total_trades;

    // Opportunities
    document.getElementById('opp-count').textContent = opps.length;
    const oppList = document.getElementById('opportunities-list');
    if (opps.length === 0) {
      oppList.innerHTML = '<div class="empty-state">Sin oportunidades activas</div>';
   } else {
      oppList.innerHTML = opps.map(o => `
        <div class="row opp-row">
         <span class="mono" style="color:var(--muted)">${o.id}</span>
         <span class="truncate" style="font-size:0.75rem">${o.opportunity_type.replace('Simple','')}</span>
         <span class="profit-badge">${fmt.usd(o.guaranteed_profit)}</span>
         <span class="status-badge ${statusClass(o.status)}">${o.status}</span>
       </div>`).join('');
    }

    // Markets
    document.getElementById('market-count').textContent = markets.length;
    const mkList = document.getElementById('markets-list');
    mkList.innerHTML = markets.slice(0,20).map(m => {
      const sumClass = m.deviation > 0.05 ? 'price-bad' : m.deviation > 0.02 ? 'price-warn' : 'price-good';
      return `
        <div class="row market-row">
         <span class="truncate" style="font-size:0.78rem">${m.question}</span>
         <span class="mono">${fmt.price(m.yes_price)}</span>
         <span class="mono">${fmt.price(m.no_price)}</span>
         <span class="mono ${sumClass}">${fmt.price(m.price_sum)}</span>
       </div>`;
    }).join('') || '<div class="empty-state">Sin mercados aún</div>';

   // Trades
    document.getElementById('trade-count').textContent = trades.length;
    const tList = document.getElementById('trades-list');
    tList.innerHTML = trades.slice(0,15).map(t => `
      <div class="row trade-row">
       <span class="mono" style="color:var(--muted)">${t.id}</span>
       <span class="mono" style="${t.actual_profit >= 0 ? 'color:var(--accent)' : 'color:var(--danger)'}">${fmt.usd(t.actual_profit)}</span>
       <span class="status-badge ${t.success ? 's-executed' : 's-failed'}">${t.success ? 'OK' : 'FAIL'}</span>
       <span style="font-size:0.75rem;color:var(--muted)">${new Date(t.executed_at).toLocaleTimeString()}</span>
     </div>`).join('') || '<div class="empty-state">Sin trades aún</div>';

 } catch(e) {
    console.error('Error fetching data:', e);
  }
}

// Actualizar cada 5 segundos
fetchAndUpdate();
setInterval(fetchAndUpdate, 5000);
</script>
</body>
</html>"#;
