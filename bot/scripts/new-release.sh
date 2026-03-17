cd :~/liquidator-bot/bot
systemctl stop liquidator
cargo build --release
systemctl restart liquidator
journalctl -u liquidator -f