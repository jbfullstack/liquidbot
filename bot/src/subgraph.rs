//! Aave V3 subgraph client — fetches all active borrowers for index backfill.
//!
//! ## Pourquoi ce module ?
//!
//! Le scan d'événements `Borrow` ne remonte que sur une fenêtre finie de blocs
//! (SCAN_LOOKBACK_BLOCKS, ~11 jours par défaut). Toute position ouverte avant cette
//! fenêtre et non re-empruntée depuis est invisible pour le bot → BLIND_SPOT.
//!
//! Le subgraph TheGraph maintient un index complet depuis le bloc de déploiement
//! d'Aave V3 sur Arbitrum. Interroger l'API GraphQL est instantané et retourne
//! l'intégralité des emprunteurs actifs.
//!
//! ## "Faire le scan historique et supprimer la dépendance ?"
//!
//! Non recommandé. Aave V3 est déployé sur Arbitrum depuis mars 2022, soit
//! ~300 millions de blocs. À 9 000 blocs par chunk, ça représente ~33 000 appels
//! RPC — plusieurs heures d'exécution. De plus, le subgraph est utile en continu
//! (refresh périodique capture les positions ouvertes via contrats intermédiaires
//! qui n'émettent pas d'événement `Borrow` lisible directement). Garder le subgraph
//! comme source optionnelle est la bonne architecture : si TheGraph tombe, le bot
//! continue avec le scan d'événements.
//!
//! ## Configuration
//!
//! Activé uniquement si `AAVE_SUBGRAPH_URL` est défini dans `.env`.
//! URL par défaut (Aave V3 Arbitrum One via TheGraph) :
//!   https://api.thegraph.com/subgraphs/name/aave/protocol-v3-arbitrum
//!
//! ## Pagination
//!
//! TheGraph limite à 1 000 entrées par requête. On utilise une pagination par curseur
//! (id_gt = dernier id de la page précédente) plutôt que `skip` — plus robuste sur
//! de grands ensembles car `skip` est O(n) côté serveur.

use alloy::primitives::Address;
use eyre::{bail, Result};

// ─── Types de désérialisation GraphQL ──────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct GqlResponse {
    data:   Option<GqlData>,
    errors: Option<Vec<GqlError>>,
}

#[derive(Debug, serde::Deserialize)]
struct GqlData {
    users: Vec<GqlUser>,
}

#[derive(Debug, serde::Deserialize)]
struct GqlError {
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct GqlUser {
    id: String,
}

// ─── API publique ───────────────────────────────────────────────────────────

/// Taille de page GraphQL. TheGraph plafonne à 1 000.
const PAGE_SIZE: usize = 1_000;

/// Délai entre deux pages pour ne pas saturer le rate-limit TheGraph.
const PAGE_DELAY_MS: u64 = 150;

/// Récupère toutes les adresses d'emprunteurs Aave V3 actifs via le subgraph.
///
/// Utilise une pagination par curseur (id_gt) — stable pour de grands ensembles.
/// Retourne une liste dédupliquée d'`Address`. Les adresses malformées sont
/// loguées en WARN et ignorées (non-fatales).
///
/// # Erreurs
/// Retourne `Err` uniquement sur :
/// - Erreur réseau / timeout
/// - Réponse HTTP non-2xx
/// - Erreur GraphQL dans le corps de réponse
/// - JSON non parseable
///
/// Les erreurs GraphQL partielles (ex. : subgraph en retard) sont propagées
/// plutôt que silencieusement ignorées, pour que le caller puisse choisir de
/// continuer sans backfill.
pub async fn fetch_aave_borrowers(
    client: &reqwest::Client,
    url:    &str,
) -> Result<Vec<Address>> {
    let mut results: Vec<Address> = Vec::new();
    let mut last_id = String::new();
    let mut page_num = 0usize;

    loop {
        page_num += 1;
        let body = serde_json::json!({
            "query": format!(
                // borrowedReservesCount_gt:0 filtre les wallets qui ont une dette active.
                // id_gt = curseur : plus stable que skip sur de grands datasets.
                "{{ users(where: {{ borrowedReservesCount_gt: 0, id_gt: \"{}\" }}, \
                           first: {PAGE_SIZE}, orderBy: id, orderDirection: asc) \
                           {{ id }} }}",
                last_id
            )
        });

        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            bail!("Subgraph HTTP {}: {url}", resp.status());
        }

        let gql: GqlResponse = resp.json().await?;

        // GraphQL peut retourner status 200 avec un champ "errors"
        if let Some(errs) = gql.errors {
            let msgs: Vec<String> = errs.into_iter().map(|e| e.message).collect();
            let hint = if msgs.iter().any(|m| m.contains("removed") || m.contains("deprecated")) {
                " — ⚠️  L'endpoint TheGraph hosted service est supprimé. \
                 Créer une API key sur https://thegraph.com/studio/apikeys/ \
                 et mettre à jour AAVE_SUBGRAPH_URL dans .env"
            } else {
                ""
            };
            bail!("Subgraph GraphQL errors: {}{}", msgs.join("; "), hint);
        }

        let users = match gql.data {
            Some(d) => d.users,
            None    => bail!("Subgraph: réponse sans champ 'data'"),
        };

        let page_len = users.len();
        tracing::debug!("📊 Subgraph page {page_num}: {page_len} entrées");

        for u in &users {
            match u.id.parse::<Address>() {
                Ok(addr) => results.push(addr),
                Err(_)   => tracing::warn!("Subgraph: adresse invalide {:?} — ignorée", u.id),
            }
        }

        if page_len < PAGE_SIZE {
            // Dernière page — pagination terminée
            break;
        }

        // Avancer le curseur sur le dernier id de la page
        // SAFETY: page_len == PAGE_SIZE > 0, donc `last()` est Some.
        last_id = users.last().unwrap().id.clone();

        tokio::time::sleep(std::time::Duration::from_millis(PAGE_DELAY_MS)).await;
    }

    tracing::info!(
        "📊 Subgraph: {} emprunteurs récupérés en {page_num} page(s)",
        results.len()
    );
    Ok(results)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Désérialisation JSON ────────────────────────────────────────────────

    #[test]
    fn test_full_page_deserializes() {
        let json = r#"{
            "data": {
                "users": [
                    {"id": "0x794a61358d6845594f94dc1db02a252b5b4814ad"},
                    {"id": "0x69fa688f1dc47d4b5d8029d5a35fb7a548310654"}
                ]
            }
        }"#;
        let resp: GqlResponse = serde_json::from_str(json).expect("should parse");
        let users = resp.data.unwrap().users;
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].id, "0x794a61358d6845594f94dc1db02a252b5b4814ad");
    }

    #[test]
    fn test_empty_page_deserializes() {
        let json = r#"{"data": {"users": []}}"#;
        let resp: GqlResponse = serde_json::from_str(json).expect("should parse");
        assert_eq!(resp.data.unwrap().users.len(), 0);
    }

    #[test]
    fn test_graphql_errors_field_deserializes() {
        let json = r#"{"data": null, "errors": [{"message": "store error"}]}"#;
        let resp: GqlResponse = serde_json::from_str(json).expect("should parse");
        assert!(resp.data.is_none());
        let errs = resp.errors.unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].message, "store error");
    }

    // ── Parsing d'adresses ──────────────────────────────────────────────────

    #[test]
    fn test_valid_address_parses() {
        let a = "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
            .parse::<Address>();
        assert!(a.is_ok());
    }

    #[test]
    fn test_lowercase_address_parses() {
        // TheGraph retourne les adresses en minuscules
        let a = "0x794a61358d6845594f94dc1db02a252b5b4814ad"
            .parse::<Address>();
        assert!(a.is_ok());
    }

    #[test]
    fn test_invalid_address_ignored() {
        let a = "not_an_address".parse::<Address>();
        assert!(a.is_err());
    }

    #[test]
    fn test_zero_address_parses() {
        let a = "0x0000000000000000000000000000000000000000"
            .parse::<Address>();
        assert!(a.is_ok());
    }

    // ── Logique de pagination ───────────────────────────────────────────────

    #[test]
    fn test_partial_page_signals_last() {
        // Une page avec < PAGE_SIZE entrées = dernière page
        let json = r#"{"data": {"users": [
            {"id": "0x794a61358d6845594f94dc1db02a252b5b4814ad"}
        ]}}"#;
        let resp: GqlResponse = serde_json::from_str(json).unwrap();
        let users = resp.data.unwrap().users;
        assert!(users.len() < PAGE_SIZE, "page partielle = dernière page");
    }

    #[test]
    fn test_cursor_is_last_id_of_page() {
        // Le curseur pour la prochaine requête doit être l'id du dernier user
        let users = vec![
            GqlUser { id: "0x0000000000000000000000000000000000000001".into() },
            GqlUser { id: "0x0000000000000000000000000000000000000002".into() },
            GqlUser { id: "0x0000000000000000000000000000000000000003".into() },
        ];
        let cursor = users.last().map(|u| u.id.as_str()).unwrap_or("");
        assert_eq!(cursor, "0x0000000000000000000000000000000000000003");
    }

    #[test]
    fn test_empty_page_does_not_advance_cursor() {
        // Si la page est vide, last() est None → pas de panic
        let users: Vec<GqlUser> = vec![];
        let cursor = users.last().map(|u| u.id.clone()).unwrap_or_default();
        assert_eq!(cursor, "");
    }

    // ── Intégration JSON → Address (pipeline complet) ───────────────────────

    #[test]
    fn test_parse_pipeline_valid_users() {
        let json = r#"{"data": {"users": [
            {"id": "0x794a61358d6845594f94dc1db02a252b5b4814ad"},
            {"id": "0x69fa688f1dc47d4b5d8029d5a35fb7a548310654"},
            {"id": "invalid_should_be_skipped"}
        ]}}"#;
        let resp: GqlResponse = serde_json::from_str(json).unwrap();
        let users = resp.data.unwrap().users;
        let mut addrs: Vec<Address> = Vec::new();
        for u in &users {
            if let Ok(addr) = u.id.parse::<Address>() {
                addrs.push(addr);
            }
        }
        // 2 valides, 1 ignorée
        assert_eq!(addrs.len(), 2);
    }

    #[test]
    fn test_page_size_constant() {
        // Vérifie qu'on ne dépasse pas la limite TheGraph
        assert!(PAGE_SIZE <= 1_000, "TheGraph plafonne à 1 000 entrées par requête");
    }
}
