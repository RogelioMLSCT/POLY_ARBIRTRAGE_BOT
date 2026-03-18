// ============================================================
// feed.rs — Conexión WebSocket en tiempo real con Polymarket
// ============================================================
//
// Este módulo:
// 1. Se conecta al WebSocket de Polymarket
// 2. Se suscribe a updates de precios
// 3. Actualiza el estado compartido (AppState) con cada mensaje
//
// WebSocket = conexión persistente bidireccional.
// En vez de hacer polling ("¿hay algo nuevo?"), el servidor
// te EMPUJA los datos cuando cambian. Latencia ~5ms vs ~50ms.
// ============================================================

use std::sync::Arc;
use anyhow::{Result, Context};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn, error, debug};
use uuid::Uuid;

use crate::types::{
   AppState, BookSnapshot, Market, MarketStatus, 
    OrderBook, OrderLevel, WsMessage
};

/// Punto de entrada del módulo feed.
/// Se llama desde main.rs y corre indefinidamente.
pub async fn run(state: Arc<AppState>) -> Result<()> {
    info!("Iniciando feed de datos de Polymarket...");

   // Loop de reconexión automática
    // Si la conexión se cae, espera 5 segundos y vuelve a conectar
    loop {
        match connect_and_stream(Arc::clone(&state)).await {
            Ok(_) => {
                warn!("WebSocket cerrado normalmente, reconectando...");
           }
            Err(e) => {
                error!("Error en WebSocket: {}. Reconectando en 5s...", e);
           }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

async fn connect_and_stream(state: Arc<AppState>) -> Result<()> {
    let ws_url = &state.config.ws_url;
    info!("Conectando a {}", ws_url);

   // Establecer conexión WebSocket
    // `connect_async` retorna (websocket_stream, http_response)
    let (mut ws_stream, _) = connect_async(ws_url)
        .await
        .context("Falló conexión WebSocket")?;

   info!("Conectado al WebSocket de Polymarket");

   // ── Suscripción a mercados ───────────────────────────────
    // Primero necesitamos obtener los mercados activos via API REST
    // y luego suscribirnos a sus condition_ids
    let market_ids = fetch_active_markets(&state).await?;
    info!("{} mercados activos encontrados", market_ids.len());

   // Enviar mensaje de suscripción al WebSocket
    // Formato específico del protocolo de Polymarket
    let subscribe_msg = json!({
        "auth": {},
       "type": "market",
       "assets_ids": market_ids.iter().take(100).collect::<Vec<_>>(), // Max 100 por suscripción
    });

    ws_stream
        .send(Message::Text(subscribe_msg.to_string()))
        .await
        .context("Falló envío de suscripción")?;

   info!("Suscripcion enviada para {} mercados", market_ids.len().min(100));

   // ── Loop principal de recepción de mensajes ──────────────
    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // Intentar parsear el mensaje JSON
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(json) => {
                        if let Err(e) = process_message(&state, json).await {
                            debug!("Error procesando mensaje: {}", e);
                       }
                    }
                    Err(e) => {
                        debug!("JSON inválido recibido: {} - {}", e, &text[..text.len().min(100)]);
                   }
                }
            }
            Ok(Message::Ping(payload)) => {
                // Responder pings para mantener la conexión viva
                ws_stream.send(Message::Pong(payload)).await?;
            }
            Ok(Message::Close(_)) => {
                info!("Servidor cerró la conexión WebSocket");
               break;
            }
            Err(e) => {
                error!("Error recibiendo mensaje: {}", e);
               break;
            }
            _ => {} // Ignorar otros tipos de mensajes (Binary, etc.)
        }
    }

    Ok(())
}

/// Procesa un mensaje del WebSocket y actualiza el estado
async fn process_message(state: &Arc<AppState>, json: serde_json::Value) -> Result<()> {
    // Polymarket envía diferentes tipos de mensajes
    let msg_type = json.get("event_type")
       .or_else(|| json.get("type"))
       .and_then(|v| v.as_str())
        .unwrap_or("unknown");

   match msg_type {
        // Book snapshot = estado completo del order book
        "book" => {
           if let Ok(snapshot) = serde_json::from_value::<BookSnapshot>(json) {
                update_order_book(state, snapshot).await;
            }
        }

        // Price change = una orden se ejecutó, los precios cambiaron
        "price_change" | "tick_size_change" => {
           if let Some(asset_id) = json.get("asset_id").and_then(|v| v.as_str()) {
               if let Some(price) = json.get("price").and_then(|v| v.as_f64()) {
                   update_market_price(state, asset_id, price).await;
                }
            }
        }

        // Market info = metadatos del mercado (nombre, estado, etc.)
        "market" | "last_trade_price" => {
           update_market_from_json(state, &json).await;
        }

        _ => {
            debug!("Tipo de mensaje desconocido: {}", msg_type);
       }
    }

    Ok(())
}

/// Actualiza el order book de un mercado con un snapshot completo
async fn update_order_book(state: &Arc<AppState>, snapshot: BookSnapshot) {
    let asset_id = snapshot.asset_id.clone();

    // Parsear los niveles del order book
    // Formato: [["0.62", "500.00"], ["0.61", "250.00"], ...]
   let parse_levels = |levels: Vec<[String; 2]>| -> Vec<OrderLevel> {
        levels.into_iter()
            .filter_map(|[price_str, size_str]| {
                let price = price_str.parse::<f64>().ok()?;
                let size = size_str.parse::<f64>().ok()?;
                Some(OrderLevel { price, size })
            })
            .collect()
    };

    let mut bids = parse_levels(snapshot.bids);
    let mut asks = parse_levels(snapshot.asks);

    // Ordenar: bids de mayor a menor, asks de menor a mayor
    bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
    asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

    // Precio mid (mejor bid + mejor ask) / 2
    let mid_price = match (bids.first(), asks.first()) {
        (Some(bid), Some(ask)) => (bid.price + ask.price) / 2.0,
        (Some(bid), None) => bid.price,
        (None, Some(ask)) => ask.price,
        (None, None) => return, // Sin datos, ignorar
    };

    // Actualizar o crear el mercado en el estado
    state.markets.entry(asset_id.clone())
        .and_modify(|market| {
            // Actualizar precios existentes
            // En Polymarket, YES y NO son tokens separados con asset_ids diferentes
            // Por simplificación, aquí asumimos que este es el token YES
            market.yes_price = mid_price;
            market.last_updated = Utc::now();
        })
        .or_insert_with(|| {
            // Crear nuevo mercado si no existe
            Market {
                condition_id: asset_id.clone(),
                question: format!("Market {}", &asset_id[..8.min(asset_id.len())]),
               yes_price: mid_price,
                no_price: 1.0 - mid_price,  // Estimación inicial
                yes_volume: asks.iter().map(|l| l.size).sum(),
                no_volume: 0.0,
                last_updated: Utc::now(),
                status: MarketStatus::Active,
            }
        });

    debug!("Order book actualizado: {} @ {:.3}", &asset_id[..8], mid_price);
}

/// Actualiza solo el precio de un mercado
async fn update_market_price(state: &Arc<AppState>, asset_id: &str, price: f64) {
   if let Some(mut market) = state.markets.get_mut(asset_id) {
        market.yes_price = price;
        market.no_price = 1.0 - price; // Actualizar NO también
        market.last_updated = Utc::now();
    }
}

/// Actualiza información del mercado desde JSON genérico
async fn update_market_from_json(state: &Arc<AppState>, json: &serde_json::Value) {
    let condition_id = match json.get("condition_id")
       .or_else(|| json.get("market"))
       .and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return,
    };

    let question = json.get("question")
       .or_else(|| json.get("description"))
       .and_then(|v| v.as_str())
        .unwrap_or("Unknown Market")
       .to_string();

    state.markets.entry(condition_id.clone())
        .and_modify(|m| {
            m.question = question.clone();
            m.last_updated = Utc::now();
        })
        .or_insert_with(|| Market {
            condition_id: condition_id.clone(),
            question,
            yes_price: 0.5,
            no_price: 0.5,
            yes_volume: 0.0,
            no_volume: 0.0,
            last_updated: Utc::now(),
            status: MarketStatus::Active,
        });
}

/// Obtiene la lista de mercados activos via API REST
/// En producción real, paginería y filtraría por criterios
async fn fetch_active_markets(state: &Arc<AppState>) -> Result<Vec<String>> {
    let api_url = &state.config.api_url;
    let markets_url = format!("{}/markets?active=true&closed=false", api_url);

   info!(" Obteniendo mercados activos de {}", markets_url);

   // En modo dry_run o sin API key, retorna mercados de ejemplo
    if state.config.api_key.is_empty() {
        warn!("Sin API key — usando mercados de ejemplo para testing");
       return Ok(example_market_ids());
    }

    let client = reqwest::Client::new();
    let response = client
        .get(&markets_url)
        .header("Authorization", format!("Bearer {}", state.config.api_key))
       .send()
        .await
        .context("Falló request a API de Polymarket")?;

   let json: serde_json::Value = response.json().await?;

    // Extraer los condition_ids del JSON de respuesta
    let ids: Vec<String> = json
        .get("data")
       .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("condition_id")?.as_str())
               .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    info!("{} mercados obtenidos de la API", ids.len());
   Ok(ids)
}

/// Mercados de ejemplo para testing sin API key real
fn example_market_ids() -> Vec<String> {
    // Estos son condition_ids reales de Polymarket (elecciones 2024)
    // Para testing local
    vec![
        "0xtest_market_1".to_string(),
       "0xtest_market_2".to_string(),
       "0xtest_market_3".to_string(),
    ]
}
