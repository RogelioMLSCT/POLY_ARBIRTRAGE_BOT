// ============================================================
// feed.rs — Conexión WebSocket en tiempo real con Polymarket
// ============================================================
//
// CORRECCIONES APLICADAS:
//
// 1. La API pública de Polymarket NO requiere autenticación
//    para leer mercados. Solo para ejecutar órdenes.
//
// 2. El WebSocket de Polymarket usa "asset_ids" (token IDs),
//    NO condition_ids. Son diferentes:
//      condition_id = ID del mercado (ej: "0xabc...")
//      asset_id     = ID del token YES o NO (ej: "123456789")
//
// 3. El formato correcto de suscripción es:
//    { "assets_ids": ["token_id_1", "token_id_2"], "type": "market" }
//
// 4. Sin API key, el bot ahora obtiene mercados reales de la
//    API pública sin autenticación.
// ============================================================

use std::sync::Arc;
use std::collections::HashMap;
use anyhow::{Result, Context};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn, error, debug};

use crate::types::{AppState, BookSnapshot, Market, MarketStatus, OrderLevel};

// ── Estructuras para parsear la API de Polymarket ────────────

#[derive(Debug, Deserialize)]
struct MarketsResponse {
    data: Option<Vec<MarketData>>,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarketData {
    condition_id: Option<String>,
    question: Option<String>,
    tokens: Option<Vec<TokenData>>,
}

#[derive(Debug, Deserialize)]
struct TokenData {
    token_id: String,
    outcome: String,
    price: Option<f64>,
}

/// Información que guardamos por cada token
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub condition_id: String,
    pub outcome: String,
    pub question: String,
}

// ── Entry point ───────────────────────────────────────────────

pub async fn run(state: Arc<AppState>) -> Result<()> {
    info!("Iniciando feed de datos de Polymarket...");

    loop {
        match connect_and_stream(Arc::clone(&state)).await {
            Ok(_) => {
                warn!("WebSocket cerrado normalmente, reconectando en 5s...");
            }
            Err(e) => {
                error!("Error en feed: {}. Reconectando en 5s...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn connect_and_stream(state: Arc<AppState>) -> Result<()> {
    // ── Paso 1: Obtener mercados via REST API publica ─────────
    info!("Obteniendo mercados activos de Polymarket...");
    let (token_ids, token_map) = fetch_markets_and_tokens(&state).await?;

    if token_ids.is_empty() {
        warn!("No se obtuvieron tokens. Reintentando en 5s...");
        return Ok(());
    }

    info!("{} tokens listos para monitorear", token_ids.len());

    // ── Paso 2: Conectar WebSocket ────────────────────────────
    let ws_url = &state.config.ws_url;
    info!("Conectando WebSocket a {}", ws_url);

    let (mut ws_stream, _) = connect_async(ws_url)
        .await
        .context("Fallo conexion WebSocket")?;

    info!("Conectado al WebSocket de Polymarket");

    // ── Paso 3: Suscribirse a tokens en lotes de 100 ─────────
    // Polymarket tiene límite de suscripciones simultáneas.
    // En dry_run usamos solo los primeros 200 tokens para no saturar.
    let tokens_to_use: Vec<String> = if state.config.dry_run {
        token_ids.into_iter().take(200).collect()
    } else {
        token_ids
    };

    let chunks: Vec<Vec<String>> = tokens_to_use.chunks(50).map(|c| c.to_vec()).collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let subscribe_msg = json!({
            "assets_ids": chunk,
            "type": "market"
        });

        ws_stream
            .send(Message::Text(subscribe_msg.to_string()))
            .await
            .context("Fallo envio de suscripcion")?;

        // Pequeño delay entre lotes para no saturar el servidor
        if i < chunks.len() - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
    }

    info!("Suscrito a {} tokens en {} lotes", tokens_to_use.len(), chunks.len());

    // ── Paso 4: Loop de recepcion de mensajes ─────────────────
    let mut msg_count = 0u64;

    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                msg_count += 1;
                if msg_count % 500 == 1 {
                    debug!("Mensajes recibidos: {}", msg_count);
                }

                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(json_val) => {
                        if let Err(e) = process_message(&state, json_val, &token_map).await {
                            debug!("Error procesando mensaje: {}", e);
                        }
                    }
                    Err(e) => {
                        debug!("JSON invalido: {} | {}", e, &text[..text.len().min(80)]);
                    }
                }
            }
            Ok(Message::Ping(payload)) => {
                ws_stream.send(Message::Pong(payload)).await?;
            }
            Ok(Message::Close(_)) => {
                info!("Servidor cerro la conexion WebSocket");
                break;
            }
            Err(e) => {
                error!("Error recibiendo mensaje: {}", e);
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

// ── Procesamiento de mensajes ─────────────────────────────────

async fn process_message(
    state: &Arc<AppState>,
    json_val: serde_json::Value,
    token_map: &HashMap<String, TokenInfo>,
) -> Result<()> {
    // Polymarket puede enviar array de eventos o un objeto unico
    if let Some(arr) = json_val.as_array() {
        for item in arr {
            process_single_event(state, item, token_map).await;
        }
    } else {
        process_single_event(state, &json_val, token_map).await;
    }
    Ok(())
}

async fn process_single_event(
    state: &Arc<AppState>,
    event: &serde_json::Value,
    token_map: &HashMap<String, TokenInfo>,
) {
    let event_type = event.get("event_type")
        .or_else(|| event.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match event_type {
        "book" => {
            if let Ok(snapshot) = serde_json::from_value::<BookSnapshot>(event.clone()) {
                update_from_book_snapshot(state, &snapshot, token_map).await;
            }
        }
        "price_change" | "last_trade_price" => {
            if let (Some(asset_id), Some(price)) = (
                event.get("asset_id").and_then(|v| v.as_str()),
                event.get("price").and_then(|v| v.as_f64()),
            ) {
                update_price_from_token(state, asset_id, price, token_map).await;
            }
        }
        _ => {
            debug!("Evento: {}", event_type);
        }
    }
}

// ── Actualizacion del estado ──────────────────────────────────

async fn update_from_book_snapshot(
    state: &Arc<AppState>,
    snapshot: &BookSnapshot,
    token_map: &HashMap<String, TokenInfo>,
) {
    let parse_levels = |levels: &Vec<[String; 2]>| -> Vec<OrderLevel> {
        levels.iter()
            .filter_map(|[p, s]| {
                Some(OrderLevel {
                    price: p.parse().ok()?,
                    size:  s.parse().ok()?,
                })
            })
            .collect()
    };

    let mut bids = parse_levels(&snapshot.bids);
    let mut asks = parse_levels(&snapshot.asks);

    bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
    asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

    let mid_price = match (bids.first(), asks.first()) {
        (Some(bid), Some(ask)) => (bid.price + ask.price) / 2.0,
        (Some(bid), None) => bid.price,
        (None, Some(ask)) => ask.price,
        (None, None) => return,
    };

    let volume: f64 = asks.iter().map(|l| l.size).sum();

    update_price_from_token(state, &snapshot.asset_id, mid_price, token_map).await;

    if let Some(info) = token_map.get(&snapshot.asset_id) {
        if let Some(mut market) = state.markets.get_mut(&info.condition_id) {
            match info.outcome.to_lowercase().as_str() {
                "yes" => market.yes_volume = volume,
                "no"  => market.no_volume  = volume,
                _ => {}
            }
        }
    }
}

async fn update_price_from_token(
    state: &Arc<AppState>,
    asset_id: &str,
    price: f64,
    token_map: &HashMap<String, TokenInfo>,
) {
    let info = match token_map.get(asset_id) {
        Some(i) => i,
        None => return,
    };

    if let Some(mut market) = state.markets.get_mut(&info.condition_id) {
        match info.outcome.to_lowercase().as_str() {
            "yes" => market.yes_price = price,
            "no"  => market.no_price  = price,
            _ => {}
        }
        market.last_updated = Utc::now();
    }
}

// ── Obtener mercados de la API publica ────────────────────────

async fn fetch_markets_and_tokens(
    state: &Arc<AppState>,
) -> Result<(Vec<String>, HashMap<String, TokenInfo>)> {
    let api_url = &state.config.api_url;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("polymarket-bot/0.1")
        .build()
        .context("Error creando cliente HTTP")?;

    let mut all_token_ids: Vec<String> = Vec::new();
    let mut token_map: HashMap<String, TokenInfo> = HashMap::new();
    let mut cursor = String::new();
    let mut page = 0;

    loop {
        page += 1;

        let url = if cursor.is_empty() {
            format!("{}/markets?active=true&closed=false&limit=100", api_url)
        } else {
            format!("{}/markets?active=true&closed=false&limit=100&next_cursor={}", api_url, cursor)
        };

        let response = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Error HTTP pagina {}: {}", page, e);
                break;
            }
        };

        if !response.status().is_success() {
            warn!("API status {} en pagina {}", response.status(), page);
            break;
        }

        let markets_resp: MarketsResponse = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Error parseando mercados: {}", e);
                break;
            }
        };

        let markets = match markets_resp.data {
            Some(m) if !m.is_empty() => m,
            _ => break,
        };

        for market in &markets {
            let condition_id = match &market.condition_id {
                Some(id) => id.clone(),
                None => continue,
            };

            let question = market.question
                .clone()
                .unwrap_or_else(|| "Sin nombre".to_string());

            if let Some(tokens) = &market.tokens {
                for token in tokens {
                    all_token_ids.push(token.token_id.clone());
                    token_map.insert(token.token_id.clone(), TokenInfo {
                        condition_id: condition_id.clone(),
                        outcome: token.outcome.clone(),
                        question: question.clone(),
                    });

                    // Guardar precio inicial si la API lo provee
                    if let Some(price) = token.price {
                        state.markets.entry(condition_id.clone())
                            .and_modify(|m| {
                                match token.outcome.to_lowercase().as_str() {
                                    "yes" => m.yes_price = price,
                                    "no"  => m.no_price  = price,
                                    _ => {}
                                }
                                m.last_updated = Utc::now();
                            })
                            .or_insert_with(|| Market {
                                condition_id: condition_id.clone(),
                                question: question.clone(),
                                yes_price: if token.outcome.to_lowercase() == "yes" { price } else { 0.5 },
                                no_price:  if token.outcome.to_lowercase() == "no"  { price } else { 0.5 },
                                yes_volume: 0.0,
                                no_volume: 0.0,
                                last_updated: Utc::now(),
                                status: MarketStatus::Active,
                            });
                    }
                }
            }
        }

        info!("Pagina {}: {} mercados | {} tokens total",
              page, markets.len(), all_token_ids.len());

        // Limitar a 200 tokens en dry_run (100 mercados aprox)
        // para no saturar el WebSocket con demasiadas suscripciones
        if state.config.dry_run && all_token_ids.len() >= 200 {
            info!("DRY_RUN: limitando a {} tokens (100 mercados)", all_token_ids.len());
            break;
        }

        match markets_resp.next_cursor {
            Some(c) if !c.is_empty() && c != "LTE=" => cursor = c,
            _ => break,
        }
    }

    info!("Total cargado: {} tokens de {} mercados",
          all_token_ids.len(), token_map.len() / 2.max(1));

    Ok((all_token_ids, token_map))
}
