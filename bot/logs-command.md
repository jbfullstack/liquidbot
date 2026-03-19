 # 1. Toutes les opportunités manquées (concurrent plus rapide)
 journalctl -u liquidator --no-pager | grep "Missed"

 # 2. Toutes les tentatives + résultats (succès / échec / skip)
 journalctl -u liquidator --no-pager | grep -E "EXECUTING|✅.*gross|❌ Revert|Simulation revert|Send failed"

 # 3. Vue complète triée par type (avec date/heure)
 journalctl -u liquidator --no-pager -o short | grep -E "Missed|EXECUTING|✅.*gross|❌ Revert|Simulation revert" | tail -100