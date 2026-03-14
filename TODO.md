 # Phase 2 (compétition avancée) : exécuter les TXs en parallèle. 
 
Actuellement on attend le receipt de user_A avant d'envoyer user_B — ça coûte plusieurs secondes.
Mais c'est un changement architectural important qu'on peut faire plus tard quand on a des liquidations réelles à analyser.


# PLAN

Analyse du problème actuel

  Block N reçu
    └─ user_A : estimate_gas (200ms) → send (50ms) → wait receipt (2000ms)
    └─ user_B : estimate_gas (200ms) → send (50ms) → wait receipt (2000ms)
    └─ user_C : estimate_gas (200ms) → send (50ms) → wait receipt (2000ms)
    Total : ~7.5 secondes pour 3 positions

  Block N+30 arrive (7.5s plus tard) — user_B et user_C déjà liquidés par d'autres bots

  Le goulot d'étranglement principal : attendre le receipt avant d'envoyer la TX suivante.

  ---
  Les 3 contraintes à résoudre

  1. Nonces — le vrai problème

  Sur Ethereum/Arbitrum, chaque TX a un nonce séquentiel obligatoire. Si deux TXs sont envoyées "simultanément" avec le même nonce → une seule passe, l'autre échoue immédiatement avec
  nonce too low.

  Alloy résout ça automatiquement via eth_getTransactionCount... mais seulement si les appels sont séquentiels. En parallèle, les deux appels retournent le même nonce.

  Solution : récupérer le nonce une seule fois, puis l'incrémenter localement avant chaque envoi.

  2. TX bloquée sur nonce gap

  Si TX nonce=5 échoue mais TX nonce=6 a été envoyée → TX nonce=6 est bloquée indéfiniment en attente que nonce=5 soit rempli.

  Solution : envoyer séquentiellement (très rapide, ~50ms/TX), ne pas attendre les receipts entre les envois.

  3. Simulation qui passe mais on-chain qui échoue

  Entre estimate_gas et l'exécution réelle, un autre bot peut avoir liquidé la position. Sur Arbitrum, gas d'une TX ratée ≈ $0.01 → acceptable.

  ---
  Architecture cible

  Block reçu
      │
      ▼
  ① SIMULATION PARALLÈLE ──────────────────────────── join_all()
     estimate_gas(user_A)  ─┐
     estimate_gas(user_B)  ─┼─► Vec<LiquidationCandidate> (filtrés, triés)
     estimate_gas(user_C)  ─┘
      │
      ▼
  ② RÉCUPÉRATION DU NONCE (une seule fois)
     get_transaction_count(wallet) → nonce_base
      │
      ▼
  ③ ENVOI SÉQUENTIEL RAPIDE ──────────────── sans await receipt
     send(user_A, nonce_base+0)   → PendingTx_A   ~50ms
     send(user_B, nonce_base+1)   → PendingTx_B   ~50ms
     send(user_C, nonce_base+2)   → PendingTx_C   ~50ms
     Total envoi : ~150ms (vs 7.5s avant)
      │
      ▼
  ④ ATTENTE RECEIPTS PARALLÈLE ──────────── join_all()
     get_receipt(PendingTx_A)  ─┐
     get_receipt(PendingTx_B)  ─┼─► Vec<Receipt>
     get_receipt(PendingTx_C)  ─┘
      │
      ▼
  ⑤ STATS + TELEGRAM (fire-and-forget, déjà OK)

  ---
  Nouvelle structure centrale : LiquidationCandidate

  Sépare proprement l'analyse de l'exécution :

  struct LiquidationCandidate {
      user:             Address,
      collateral:       Address,
      collateral_sym:   &'static str,
      debt:             Address,
      debt_sym:         &'static str,
      debt_amount:      U256,
      debt_usd:         f64,
      hf_display:       f64,
      close_pct:        u8,
      close_fraction:   f64,
      gas_estimate:     u64,
      gas_usd:          f64,
      expected_profit:  f64,
      min_profit:       U256,
      fee_tier:         u32,
  }

  ---
  Plan d'implémentation en 4 étapes

  Étape 1 — Extraire build_candidate()

  Fonction pure qui prend un user: Address et retourne Option<LiquidationCandidate> :
  - fetch getUserAccountData
  - fetch reserves (debt + collateral)
  - calcul gas, profit estimé, filtres
  - retourne None si simulation revert ou non profitable

  Étape 2 — Simulation parallèle avec limite de concurrence

  // Max 10 simulations en parallèle pour ne pas saturer drpc.org
  use futures_util::stream::{self, StreamExt};
  let candidates: Vec<LiquidationCandidate> = stream::iter(liquidatable_users)
      .map(|user| build_candidate(user, ...))
      .buffer_unordered(10)
      .filter_map(|r| async { r })
      .collect()
      .await;

  Étape 3 — Envoi séquentiel avec nonce explicite

  let nonce_base = http_ro.get_transaction_count(wallet_addr).await?;
  let mut pending_txs: Vec<(PendingTransaction, LiquidationCandidate)> = Vec::new();

  for (i, candidate) in candidates.iter().enumerate() {
      let tx = liquidator.liquidate(...)
          .nonce(nonce_base + i as u64)
          .gas(candidate.gas_estimate * 13 / 10);

      match tx.send().await {
          Ok(p)  => pending_txs.push((p, candidate.clone())),
          Err(e) => warn!("send failed for {}: {e}", candidate.user),
      }
      // Pas de sleep, pas d'await receipt — on enchaîne immédiatement
  }

  Étape 4 — Receipts parallèles

  let results = join_all(
      pending_txs.iter().map(|(p, c)| async move {
          (c, p.get_receipt().await)
      })
  ).await;

  for (candidate, receipt_result) in results {
      // record_stats + notify_telegram (déjà fire-and-forget)
  }

  ---
  Gains attendus

  ┌─────────────────────────┬─────────┬──────────────────┐
  │        Scénario         │  Avant  │      Après       │
  ├─────────────────────────┼─────────┼──────────────────┤
  │ 1 position liquidable   │ ~2.3s   │ ~2.3s (inchangé) │
  ├─────────────────────────┼─────────┼──────────────────┤
  │ 3 positions liquidables │ ~7.5s   │ ~2.5s            │
  ├─────────────────────────┼─────────┼──────────────────┤
  │ 5 positions (crash)     │ ~12.5s  │ ~3.0s            │
  ├─────────────────────────┼─────────┼──────────────────┤
  │ Positions manquées/bloc │ 4 sur 5 │ 0 sur 5          │
  └─────────────────────────┴─────────┴──────────────────┘

  ---
  Risques résiduels

  ┌──────────────────────────┬─────────────┬──────────────────────┬────────────────────────────────────────────┐
  │          Risque          │ Probabilité │        Impact        │                 Mitigation                 │
  ├──────────────────────────┼─────────────┼──────────────────────┼────────────────────────────────────────────┤
  │ Nonce gap si TX_N échoue │ Moyen       │ TX_N+1 bloquée       │ Timeout de 30s sur receipt, log + continue │
  ├──────────────────────────┼─────────────┼──────────────────────┼────────────────────────────────────────────┤
  │ Rate limit drpc.org      │ Faible      │ Simulations ignorées │ buffer_unordered(10)                       │
  ├──────────────────────────┼─────────────┼──────────────────────┼────────────────────────────────────────────┤
  │ 2 TX pour le même user   │ Impossible  │ —                    │ pending_liquidations HashSet inchangé      │
  ├──────────────────────────┼─────────────┼──────────────────────┼────────────────────────────────────────────┤
  │ Gas gaspillé sur échecs  │ Certain     │ ~$0.01/TX ratée      │ Acceptable sur Arbitrum                    │
  └──────────────────────────┴─────────────┴──────────────────────┴────────────────────────────────────────────┘

  ---
  Ce qu'on NE fait PAS

  - Pas de gestion de remplacement de TX (replace-by-fee) — inutile sur Arbitrum FCFS
  - Pas de parallelisme sur l'envoi lui-même — le nonce rend ça plus complexe que le gain
  - Pas de MEV/flashbots — hors scope Phase 2

  ---
  Quand implémenter ? Quand tu auras eu une première vraie liquidation et que tu pourras confirmer que le bot fonctionne end-to-end. La Phase 2 optimise la capture, pas la correction.
