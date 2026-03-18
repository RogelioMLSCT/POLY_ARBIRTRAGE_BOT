// ============================================================
// math.rs — Motor matemático: Bregman + Frank-Wolfe
// ============================================================
//
// Este es el corazón del bot. Implementa:
//
// 1. DIVERGENCIA DE BREGMAN (KL Divergence para LMSR)
//    D(μ||θ) = Σ μᵢ × ln(μᵢ / pᵢ(θ))
//    Mide "distancia" entre precios actuales y precios correctos
//
// 2. PROYECCIÓN DE BREGMAN
//   μ* = argmin D(μ||θ) sujeto a μ ∈ M (politopo marginal)
//    Encuentra los precios correctos más cercanos
//
// 3. FRANK-WOLFE ALGORITHM
//    Encuentra la proyección iterativamente sin enumerar 2^63 outcomes
//
// 4. CRITERIO DE PARADA (Proposición 4.1)
//    Ganancia garantizada = D(μ||θ) - g(μ)
//    Para cuando g(μ) ≤ (1-α) × D(μ||θ)
// ============================================================

use tracing::debug;

/// Un vector de precios de mercado
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PriceVector {
    pub prices: Vec<f64>,
    pub labels: Vec<String>,  // Para debugging: qué representa cada precio
}

impl PriceVector {
    pub fn new(prices: Vec<f64>, labels: Vec<String>) -> Self {
        PriceVector { prices, labels }
    }

    /// Suma de todos los precios (debería ser ~1.0 si es arbitrage-free)
    pub fn sum(&self) -> f64 {
        self.prices.iter().sum()
    }

    /// Número de outcomes
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// Punto interior (promedio de todos los precios)
    /// Garantiza que todas las coordenadas están entre 0 y 1
    pub fn interior_point(price_vectors: &[PriceVector]) -> Vec<f64> {
        if price_vectors.is_empty() {
            return vec![];
        }
        let n = price_vectors[0].len();
        let m = price_vectors.len() as f64;

        (0..n).map(|i| {
            price_vectors.iter().map(|v| v.prices[i]).sum::<f64>() / m
        }).collect()
    }
}

// ── Divergencia de Bregman (KL Divergence) ──────────────────

/// D(mu || theta) para LMSR
///
/// Parámetros:
///   mu: vector de precios objetivo (arbitrage-free)
///   theta: vector de precios actuales del mercado
///
/// Retorna: la divergencia (≥ 0, mayor = más arbitraje disponible)
pub fn bregman_divergence(mu: &[f64], theta: &[f64]) -> f64 {
    assert_eq!(mu.len(), theta.len(), "Vectores de diferente tamaño");

   mu.iter().zip(theta.iter())
        .filter(|(&m, &t)| m > 1e-10 && t > 1e-10) // Evitar ln(0) = -∞
        .map(|(&m, &t)| {
            // KL divergence: μᵢ × ln(μᵢ / θᵢ)
            m * (m / t).ln()
        })
        .sum()
}

/// Gradiente de R(μ) = Σ μᵢ ln(μᵢ) (entropía negativa)
/// ∇R(μ)ᵢ = ln(μᵢ) + 1
///
/// NOTA: Este gradiente explota cuando μᵢ → 0 (ln(0) = -∞)
/// Por eso usamos contracción adaptativa
pub fn bregman_gradient(mu: &[f64]) -> Vec<f64> {
    mu.iter().map(|&m| {
        if m > 1e-12 {
            m.ln() + 1.0
        } else {
            -1e10 // Clamping para evitar overflow
        }
    }).collect()
}

// ── Contracción Adaptativa ───────────────────────────────────

/// Aplica contracción al politopo para evitar gradiente explosivo
///
/// M' = (1-ε)M + ε×u
/// Cada vértice v se convierte en: v' = (1-ε)v + ε×u
///
/// Esto aleja los vértices de los bordes (donde μᵢ = 0)
/// manteniendo el gradiente finito.
pub fn contract_polytope_vertex(vertex: &[f64], interior: &[f64], epsilon: f64) -> Vec<f64> {
    vertex.iter().zip(interior.iter())
        .map(|(&v, &u)| (1.0 - epsilon) * v + epsilon * u)
        .collect()
}

/// Regla adaptativa de epsilon (del paper de Kroer et al.)
///
/// Si el progreso es bueno → reducir epsilon (acercarse al M real)
/// Si el progreso es lento → mantener epsilon
pub fn update_epsilon(
    current_epsilon: f64,
    fw_gap: f64,
    interior_gap: f64,
) -> f64 {
    let ratio = fw_gap / (-4.0 * interior_gap).abs();

    if ratio < current_epsilon {
        // Reducir epsilon: tomamos el mínimo entre ratio y epsilon/2
        ratio.min(current_epsilon / 2.0).max(1e-8) // Mínimo 1e-8 para estabilidad
    } else {
        // Mantener epsilon
        current_epsilon
    }
}

// ── Frank-Wolfe Algorithm ────────────────────────────────────

/// Resultado de una iteración de Frank-Wolfe
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FWResult {
    /// Precios proyectados (μ* aproximado)
    pub projected_prices: Vec<f64>,
    /// Divergencia de Bregman D(μ||θ) = ganancia máxima posible
    pub divergence: f64,
    /// Frank-Wolfe gap g(μ) = error de aproximación
    pub fw_gap: f64,
    /// Ganancia garantizada = D - g
    pub guaranteed_profit: f64,
    /// Número de iteraciones realizadas
    pub iterations: usize,
    /// ¿Convergió según el criterio α?
    pub converged: bool,
}

/// Vértice del politopo marginal (un outcome válido)
#[derive(Debug, Clone)]
pub struct Vertex {
    /// Vector binario: qué outcomes son verdaderos en este escenario
    pub values: Vec<f64>,
}

/// Restricciones del politopo marginal
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MarketConstraints {
    /// Grupos de outcomes mutuamente excluyentes
    /// Ej: [[YES, NO]] → YES + NO = 1
    pub exclusive_groups: Vec<Vec<usize>>,
    /// Restricciones de implicación: si i=1, entonces j=1
    pub implications: Vec<(usize, usize)>,
}

impl MarketConstraints {
    /// Restricciones simples para mercado YES/NO
    pub fn simple_yes_no() -> Self {
        MarketConstraints {
            exclusive_groups: vec![vec![0, 1]], // YES y NO son mutuamente excluyentes
            implications: vec![],
        }
    }

    /// Verifica si un vector de precios es factible (satisface las restricciones)
    pub fn is_feasible(&self, prices: &[f64], tolerance: f64) -> bool {
        for group in &self.exclusive_groups {
            let sum: f64 = group.iter().map(|&i| prices[i]).sum();
            if (sum - 1.0).abs() > tolerance {
                return false;
            }
        }
        true
    }

    /// Genera los vértices del politopo para grupos mutuamente excluyentes
    /// Para YES/NO: [[1.0, 0.0], [0.0, 1.0]]
    pub fn generate_vertices(&self, n: usize) -> Vec<Vertex> {
        let mut vertices = vec![];

        for group in &self.exclusive_groups {
            for &active_idx in group {
                let mut values = vec![0.0f64; n];
                values[active_idx] = 1.0;
                vertices.push(Vertex { values });
            }
        }

        vertices
    }
}

/// Frank-Wolfe con Barrera Adaptativa
///
/// Resuelve: μ* = argmin D(μ||θ) sujeto a μ ∈ M
///
/// Parámetros:
///   theta:       precios actuales del mercado
///   constraints: restricciones del politopo M
///   alpha:       criterio de parada (0.9 = captura 90% del arbitraje)
///   max_iter:    iteraciones máximas
pub fn frank_wolfe_project(
    theta: &[f64],
    constraints: &MarketConstraints,
    alpha: f64,
    max_iter: usize,
) -> FWResult {
    let n = theta.len();

    // ── Inicialización (InitFW) ──────────────────────────────
    // Generar vértices iniciales del politopo
    let vertices = constraints.generate_vertices(n);

    if vertices.is_empty() {
        return FWResult {
            projected_prices: theta.to_vec(),
            divergence: 0.0,
            fw_gap: 0.0,
            guaranteed_profit: 0.0,
            iterations: 0,
            converged: true,
        };
    }

    // Punto interior u = promedio de todos los vértices
    // Garantiza que todas las coordenadas están estrictamente entre 0 y 1
    let interior: Vec<f64> = (0..n).map(|i| {
        vertices.iter().map(|v| v.values[i]).sum::<f64>() / vertices.len() as f64
    }).collect();

    // Epsilon inicial (10% de contracción)
    let mut epsilon = 0.1_f64;

    // μ₀ = interior point como punto de inicio
    let mut mu: Vec<f64> = interior.clone();

    // Conjunto activo inicial (coeficientes de combinación convexa)
    let _active_set: Vec<(Vertex, f64)> = vec![(
        Vertex { values: interior.clone() },
        1.0,
    )];

    let mut best_guaranteed_profit = 0.0_f64;
    let mut best_mu = mu.clone();
    let mut best_fw_gap = f64::INFINITY;

    // Gap en el punto interior (para la regla de epsilon adaptativo)
    let interior_gap = compute_fw_gap(&interior, theta, constraints, epsilon, &interior);

    for iter in 0..max_iter {
        // ── Paso 1: Calcular gradiente ──────────────────────
        // ∇F(μ) = ∇D(μ||θ) = ∇R(μ) = ln(μ) + 1
        let gradient = bregman_gradient(&mu);

        // ── Paso 2: Encontrar vértice de descenso ───────────
        // z = argmin_{z ∈ M'} ∇F(μ) · z
        // (Linear Minimization Oracle - LMO)
        // En vez de resolver un IP completo (Gurobi), usamos el método
        // analítico para politopos simples: elegir el vértice que minimiza
        // el producto punto con el gradiente
        let descent_vertex = linear_min_oracle(
            &gradient,
            constraints,
            &interior,
            epsilon,
            n,
        );

        // ── Paso 3: Calcular Frank-Wolfe gap ────────────────
        // g(μ) = ∇F(μ) · (μ - z*)
        let fw_gap: f64 = gradient.iter().zip(mu.iter()).zip(descent_vertex.iter())
            .map(|((&g, &m), &z)| g * (m - z))
            .sum();

        // ── Paso 4: Calcular divergencia y ganancia garantizada
        let divergence = bregman_divergence(&mu, theta);
        let guaranteed_profit = (divergence - fw_gap).max(0.0);

        debug!(
            "FW iter {}: D={:.6}, g={:.6}, profit_garantizado={:.6}, ε={:.6}",
           iter, divergence, fw_gap, guaranteed_profit, epsilon
        );

        // Guardar mejor resultado hasta ahora
        if guaranteed_profit > best_guaranteed_profit {
            best_guaranteed_profit = guaranteed_profit;
            best_mu = mu.clone();
            best_fw_gap = fw_gap;
        }

        // ── Criterio de parada α-extracción ─────────────────
        // Para cuando: g(μ) ≤ (1-α) × D(μ||θ)
        // Es decir: cuando la ganancia garantizada ≥ α × D(μ||θ)
        if fw_gap <= (1.0 - alpha) * divergence {
            debug!("Convergencia en iteración {} (criterio α={:.1})", iter, alpha);
           return FWResult {
                projected_prices: mu,
                divergence,
                fw_gap,
                guaranteed_profit,
                iterations: iter + 1,
                converged: true,
            };
        }

        // ── Paso 5: Actualizar epsilon adaptativo ───────────
        epsilon = update_epsilon(epsilon, fw_gap, interior_gap);

        // ── Paso 6: Paso de Frank-Wolfe ──────────────────────
        // γ = step size óptimo (line search exacto para KL divergence)
        // Para simplificar, usamos step size estándar γ = 2/(t+2)
        let gamma = 2.0 / (iter as f64 + 2.0);

        // μ_{t+1} = (1-γ)μ_t + γ×z_t
        for i in 0..n {
            mu[i] = (1.0 - gamma) * mu[i] + gamma * descent_vertex[i];
            // Proyectar al intervalo [ε, 1-ε] para estabilidad numérica
            mu[i] = mu[i].max(epsilon * 0.01).min(1.0 - epsilon * 0.01);
        }
    }

    // Iteraciones agotadas — retornar mejor resultado encontrado
    let final_divergence = bregman_divergence(&best_mu, theta);
    FWResult {
        projected_prices: best_mu,
        divergence: final_divergence,
        fw_gap: best_fw_gap,
        guaranteed_profit: best_guaranteed_profit,
        iterations: max_iter,
        converged: false,
    }
}

/// Linear Minimization Oracle (LMO)
///
/// Encuentra el vértice z* de M' que minimiza ∇F(μ) · z
/// Para politopos simples (YES/NO), esto es analítico:
/// Elegir el outcome con menor gradiente en cada grupo exclusivo
fn linear_min_oracle(
    gradient: &[f64],
    constraints: &MarketConstraints,
    interior: &[f64],
    epsilon: f64,
    n: usize,
) -> Vec<f64> {
    let mut z = vec![0.0f64; n];

    // Para cada grupo mutuamente excluyente,
    // asignar 1.0 al outcome con menor gradiente (mayor descenso)
    for group in &constraints.exclusive_groups {
        let min_idx = group.iter()
            .min_by(|&&i, &&j| {
                gradient[i].partial_cmp(&gradient[j])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
            .unwrap_or(group[0]);

        z[min_idx] = 1.0;
    }

    // Aplicar contracción: z' = (1-ε)z + ε×u
    contract_polytope_vertex(&z, interior, epsilon)
}

/// Calcula el Frank-Wolfe gap en un punto dado
fn compute_fw_gap(
    mu: &[f64],
    _theta: &[f64],
    constraints: &MarketConstraints,
    epsilon: f64,
    interior: &[f64],
) -> f64 {
    let gradient = bregman_gradient(mu);
    let n = mu.len();
    let descent = linear_min_oracle(&gradient, constraints, interior, epsilon, n);

    gradient.iter().zip(mu.iter()).zip(descent.iter())
        .map(|((&g, &m), &z)| g * (m - z))
        .sum()
}

// ── Detección de Arbitraje Simple ───────────────────────────

/// Resultado del análisis de un par YES/NO
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SimpleArbitrageAnalysis {
    pub market_id: String,
    pub yes_price: f64,
    pub no_price: f64,
    pub price_sum: f64,
    pub arbitrage_type: SimpleArbitrageType,
    pub gross_profit_per_dollar: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SimpleArbitrageType {
    /// YES + NO < 1.0 → comprar ambos
    Underpriced { profit: f64 },
    /// YES + NO > 1.0 → vender ambos
    Overpriced { profit: f64 },
    /// No hay arbitraje
    None,
}

/// Analiza un mercado simple YES/NO para arbitraje
pub fn analyze_simple_arbitrage(
    market_id: &str,
    yes_price: f64,
    no_price: f64,
    min_threshold: f64,
) -> SimpleArbitrageAnalysis {
    let price_sum = yes_price + no_price;
    let _deviation = (price_sum - 1.0).abs();

    let arbitrage_type = if price_sum < 1.0 - min_threshold {
        // Comprar ambos: pagas `price_sum`, recibes $1 → ganancia = 1 - price_sum
        SimpleArbitrageType::Underpriced { profit: 1.0 - price_sum }
    } else if price_sum > 1.0 + min_threshold {
        // Vender ambos: recibes `price_sum`, pagas $1 → ganancia = price_sum - 1
        SimpleArbitrageType::Overpriced { profit: price_sum - 1.0 }
    } else {
        SimpleArbitrageType::None
    };

    let gross_profit = match &arbitrage_type {
        SimpleArbitrageType::Underpriced { profit } => *profit,
        SimpleArbitrageType::Overpriced { profit } => *profit,
        SimpleArbitrageType::None => 0.0,
    };

    SimpleArbitrageAnalysis {
        market_id: market_id.to_string(),
        yes_price,
        no_price,
        price_sum,
        arbitrage_type,
        gross_profit_per_dollar: gross_profit,
    }
}

// ── Kelly Criterion para Position Sizing ────────────────────

/// Calcula el tamaño óptimo de posición usando Kelly modificado
///
/// f = (b×p - q) / b × sqrt(p)
///
/// Donde:
///   b = profit percentage (ej: 0.10 para 10% ganancia)
///   p = probabilidad de ejecución completa
///   q = 1 - p
///   max_capital = capital máximo disponible
pub fn kelly_position_size(
    profit_pct: f64,
    fill_probability: f64,
    max_capital: f64,
    order_book_depth: f64,
) -> f64 {
    let b = profit_pct;
    let p = fill_probability.max(0.01).min(0.99);
    let q = 1.0 - p;

    // Kelly fraction
    let kelly_f = if b > 0.0 {
        ((b * p - q) / b) * p.sqrt()
    } else {
        0.0
    };

    // Limitar al 50% de la profundidad del order book
    // (para no mover el mercado en contra tuya)
    let max_from_book = order_book_depth * 0.5;

    // Tomar el mínimo entre Kelly, capital máximo, y liquidez disponible
    (kelly_f * max_capital)
        .max(0.0)
        .min(max_capital)
        .min(max_from_book)
}

// ── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bregman_divergence_identical() {
        // D(μ||μ) = 0 (distancia de un vector consigo mismo)
        let mu = vec![0.6, 0.4];
        let d = bregman_divergence(&mu, &mu);
        assert!(d < 1e-10, "D(μ||μ) debe ser 0, fue {}", d);
   }

    #[test]
    fn test_bregman_divergence_positive() {
        // D(μ||θ) ≥ 0 siempre
        let mu = vec![0.5, 0.5];
        let theta = vec![0.62, 0.33]; // Suma = 0.95 (hay arbitraje)
        let d = bregman_divergence(&mu, &theta);
        assert!(d >= 0.0, "Bregman divergence debe ser no-negativa");
   }

    #[test]
    fn test_simple_arbitrage_underpriced() {
        // YES=0.40, NO=0.45 → suma=0.85, hay arbitraje
        let result = analyze_simple_arbitrage("test_market", 0.40, 0.45, 0.02);
       assert!(matches!(result.arbitrage_type, SimpleArbitrageType::Underpriced { .. }));
        assert!((result.gross_profit_per_dollar - 0.15).abs() < 1e-10);
    }

    #[test]
    fn test_simple_arbitrage_overpriced() {
        // YES=0.60, NO=0.55 → suma=1.15, hay arbitraje al vender
        let result = analyze_simple_arbitrage("test_market", 0.60, 0.55, 0.02);
       assert!(matches!(result.arbitrage_type, SimpleArbitrageType::Overpriced { .. }));
    }

    #[test]
    fn test_simple_arbitrage_none() {
        // YES=0.60, NO=0.40 → suma=1.0, sin arbitraje
        let result = analyze_simple_arbitrage("test_market", 0.60, 0.40, 0.02);
       assert!(matches!(result.arbitrage_type, SimpleArbitrageType::None));
    }

    #[test]
    fn test_frank_wolfe_simple() {
        // Precios con arbitraje obvio: sum = 0.80
        let theta = vec![0.50, 0.30]; // YES=0.50, NO=0.30 → suma=0.80
        let constraints = MarketConstraints::simple_yes_no();

        let result = frank_wolfe_project(&theta, &constraints, 0.9, 100);

        // La proyección debería sumar ~1.0
        let projected_sum: f64 = result.projected_prices.iter().sum();
        assert!(
            (projected_sum - 1.0).abs() < 0.05,
            "Proyección suma {:.3}, debería ser ~1.0", projected_sum
       );

        // Debería haber ganancia positiva
        assert!(
            result.guaranteed_profit > 0.0,
            "Debería detectar arbitraje, profit={:.6}", result.guaranteed_profit
       );
    }

    #[test]
    fn test_kelly_criterion() {
        let size = kelly_position_size(0.10, 0.85, 1000.0, 500.0);
        assert!(size >= 0.0);
        assert!(size <= 1000.0);
        println!("Kelly size: ${:.2}", size);
    }
}
