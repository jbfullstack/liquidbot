# Setup systemd pour le bot

## 1. Fichier service

Crée `/etc/systemd/system/liquidator.service` :

```ini
[Unit]
Description=Aave V3 Liquidation Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=liquidator
WorkingDirectory=/home/liquidator/liquidator-bot/bot
EnvironmentFile=/home/liquidator/liquidator-bot/bot/.env
ExecStart=/home/liquidator/liquidator-bot/bot/target/release/liquidator-bot
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable liquidator
sudo systemctl start liquidator
```

## 2. Commande /restart depuis Telegram

La commande `/restart` dans Telegram appelle `systemctl restart liquidator`.

### Option A — bot tourne en root (simple, pas recommandé en prod)

Ça marche directement, rien à configurer.

### Option B — bot tourne en user dédié (recommandé)

Donne la permission de restart sans mot de passe via sudoers :

```bash
# Crée le fichier /etc/sudoers.d/liquidator-restart
echo "liquidator ALL=(root) NOPASSWD: /bin/systemctl restart liquidator" \
  | sudo tee /etc/sudoers.d/liquidator-restart
sudo chmod 440 /etc/sudoers.d/liquidator-restart
```

Puis change la commande dans `.env` pour utiliser sudo :

```env
# Si le bot ne tourne pas en root, préfixe avec sudo
SERVICE_NAME=liquidator
```

Et dans ce cas, modifie la ligne dans le bot :
```rust
// Remplace "systemctl" par "sudo" et ajoute "systemctl" en premier arg
tokio::process::Command::new("sudo")
    .arg("systemctl")
    .arg("restart")
    .arg(&service)
    .spawn()
```

### Option C — script wrapper setuid (alternative propre)

```bash
cat > /usr/local/bin/restart-liquidator.sh << 'EOF'
#!/bin/bash
exec /bin/systemctl restart liquidator
EOF
chmod +x /usr/local/bin/restart-liquidator.sh
chown root:liquidator /usr/local/bin/restart-liquidator.sh
chmod u+s /usr/local/bin/restart-liquidator.sh  # setuid root
```

Puis dans `.env` :
```env
SERVICE_NAME=/usr/local/bin/restart-liquidator.sh
```

Et dans le bot, au lieu de `systemctl restart $SERVICE`, appelle directement le script.

## 3. Vérifier les logs

```bash
journalctl -u liquidator -f          # logs en temps réel
journalctl -u liquidator -n 100      # 100 dernières lignes
systemctl status liquidator          # état actuel
```
