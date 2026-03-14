#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# deploy-smart-contract.sh
# Déploie FlashLiquidator sur Arbitrum One.
# Utilise les variables du fichier bot/.env
#
# Usage :
#   chmod +x deploy-smart-contract.sh   (une seule fois)
#   ./deploy-smart-contract.sh
# ─────────────────────────────────────────────────────────────

set -euo pipefail

# ── Chemins ─────────────────────────────────────────────────
CONTRACTS_DIR="$(cd "$(dirname "$0")" && pwd)"
ENV_FILE="$CONTRACTS_DIR/../bot/.env"

# ── Charge le .env ──────────────────────────────────────────
if [ ! -f "$ENV_FILE" ]; then
  echo "ERREUR : fichier .env introuvable à $ENV_FILE"
  exit 1
fi

export $(grep -E '^(PRIVATE_KEY|COLD_WALLET|ARBITRUM_RPC_URL)=' "$ENV_FILE" | xargs)

# ── Vérifie que les variables sont présentes ────────────────
[ -z "${PRIVATE_KEY:-}"       ] && echo "ERREUR : PRIVATE_KEY manquant dans .env"       && exit 1
[ -z "${COLD_WALLET:-}"       ] && echo "ERREUR : COLD_WALLET manquant dans .env"       && exit 1
[ -z "${ARBITRUM_RPC_URL:-}"  ] && echo "ERREUR : ARBITRUM_RPC_URL manquant dans .env"  && exit 1

# ── Infos avant déploiement ─────────────────────────────────
DEPLOYER=$(cast wallet address --private-key "$PRIVATE_KEY")
BALANCE=$(cast balance "$DEPLOYER" --rpc-url "$ARBITRUM_RPC_URL" --ether)

echo ""
echo "  FlashLiquidator — DÉPLOIEMENT Arbitrum One"
echo "  ─────────────────────────────────────────────────────"
echo "  Deployer    : $DEPLOYER"
echo "  Balance     : $BALANCE ETH"
echo "  Cold wallet : $COLD_WALLET"
echo "  RPC         : $ARBITRUM_RPC_URL"
echo "  ─────────────────────────────────────────────────────"
echo ""

read -p "  Confirmer le déploiement ? (oui/non) : " CONFIRM
[ "$CONFIRM" != "oui" ] && echo "  Annulé." && exit 0

# ── Déploiement ─────────────────────────────────────────────
echo ""
echo "  Déploiement en cours..."
echo ""

# On se place dans contracts/ pour que forge trouve les chemins correctement
cd "$CONTRACTS_DIR"

# Forge tourne directement (pas capturé) → output visible en temps réel
forge script script/Deploy.s.sol \
  --rpc-url "$ARBITRUM_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --broadcast \
  -vvv

# ── Récupère l'adresse depuis le fichier broadcast de forge ─
# Forge écrit automatiquement broadcast/Deploy.s.sol/42161/run-latest.json
BROADCAST="$CONTRACTS_DIR/broadcast/Deploy.s.sol/42161/run-latest.json"

if [ ! -f "$BROADCAST" ]; then
  echo ""
  echo "  ⚠ Fichier broadcast introuvable : $BROADCAST"
  echo "  Cherche l'adresse manuellement sur :"
  echo "  https://arbiscan.io/address/$DEPLOYER"
  exit 0
fi

CONTRACT_ADDRESS=$(grep -o '"contractAddress":"0x[^"]*"' "$BROADCAST" \
  | head -1 \
  | grep -o '0x[^"]*')

if [ -z "$CONTRACT_ADDRESS" ]; then
  echo ""
  echo "  ⚠ Impossible d'extraire l'adresse depuis le broadcast."
  echo "  Cherche sur : https://arbiscan.io/address/$DEPLOYER"
  exit 0
fi

# ── Met à jour CONTRACT_ADDRESS dans le .env ────────────────
# if grep -q '^CONTRACT_ADDRESS=' "$ENV_FILE"; then
#   sed -i "s|^CONTRACT_ADDRESS=.*|CONTRACT_ADDRESS=$CONTRACT_ADDRESS|" "$ENV_FILE"
# else
#   echo "CONTRACT_ADDRESS=$CONTRACT_ADDRESS" >> "$ENV_FILE"
# fi

echo ""
echo "  ✓ Contrat déployé   : $CONTRACT_ADDRESS"
# echo "  ✓ .env mis à jour   : CONTRACT_ADDRESS=$CONTRACT_ADDRESS"
echo "  !!! UPDATE .env !!! : CONTRACT_ADDRESS=$CONTRACT_ADDRESS"
echo "  Vérifie sur         : https://arbiscan.io/address/$CONTRACT_ADDRESS"
echo ""
