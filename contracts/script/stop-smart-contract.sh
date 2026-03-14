#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# stop-smart-contract.sh
# Met le FlashLiquidator en pause (setPaused(true)) en urgence.
# Utilise les variables du fichier .env du bot.
#
# Usage :
#   chmod +x stop-smart-contract.sh   (une seule fois)
#   ./stop-smart-contract.sh
# ─────────────────────────────────────────────────────────────

set -e

# ── Charge le .env ──────────────────────────────────────────
ENV_FILE="$(dirname "$0")/../bot/.env"

if [ ! -f "$ENV_FILE" ]; then
  echo "ERREUR : fichier .env introuvable à $ENV_FILE"
  exit 1
fi

# Exporte uniquement les variables dont on a besoin
export $(grep -E '^(PRIVATE_KEY|CONTRACT_ADDRESS|ARBITRUM_RPC_URL)=' "$ENV_FILE" | xargs)

# ── Vérifie que les variables sont présentes ────────────────
if [ -z "$PRIVATE_KEY" ]; then
  echo "ERREUR : PRIVATE_KEY manquant dans .env"
  exit 1
fi
if [ -z "$CONTRACT_ADDRESS" ]; then
  echo "ERREUR : CONTRACT_ADDRESS manquant dans .env"
  exit 1
fi

# RPC : utilise celui du .env, ou le public Arbitrum en fallback
RPC="${ARBITRUM_RPC_URL:-https://arb1.arbitrum.io/rpc}"

# ── Affiche ce qu'on va faire ───────────────────────────────
echo ""
echo "  FlashLiquidator PAUSE"
echo "  ─────────────────────────────────────────"
echo "  Contrat  : $CONTRACT_ADDRESS"
echo "  RPC      : $RPC"
echo ""
read -p "  Confirmer la mise en pause ? (oui/non) : " CONFIRM

if [ "$CONFIRM" != "oui" ]; then
  echo "  Annulé."
  exit 0
fi

# ── Envoie la transaction ───────────────────────────────────
echo ""
echo "  Envoi de setPaused(true)..."

TX=$(cast send "$CONTRACT_ADDRESS" \
  "setPaused(bool)" true \
  --private-key "$PRIVATE_KEY" \
  --rpc-url "$RPC" \
  --json | grep -o '"transactionHash":"[^"]*"' | cut -d'"' -f4)

echo ""
echo "  ✓ Transaction envoyée : $TX"
echo "  Vérifie sur : https://arbiscan.io/tx/$TX"
echo ""

# ── Vérifie l'état après pause ──────────────────────────────
PAUSED=$(cast call "$CONTRACT_ADDRESS" \
  "paused()(bool)" \
  --rpc-url "$RPC")

echo "  État paused : $PAUSED"

if [ "$PAUSED" = "true" ]; then
  echo "  ✓ Contrat en pause. Le bot ne peut plus liquider."
else
  echo "  ⚠ Vérification échouée — contrôle manuellement sur arbiscan.io"
fi

echo ""
