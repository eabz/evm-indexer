ALTER TABLE blocks ADD PROJECTION blocks_count_by_chain (SELECT count(*) as blocks, chain GROUP BY chain);

ALTER TABLE blocks
    MATERIALIZE PROJECTION blocks_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE contracts ADD PROJECTION contracts_count_by_chain (SELECT count(*) as contracts, chain GROUP BY chain);

ALTER TABLE contracts
    MATERIALIZE PROJECTION contracts_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE logs ADD PROJECTION logs_count_by_chain (SELECT count(*) as logs, chain GROUP BY chain);

ALTER TABLE logs
    MATERIALIZE PROJECTION logs_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE traces ADD PROJECTION traces_count_by_chain (SELECT count(*) as traces, chain GROUP BY chain);

ALTER TABLE traces
    MATERIALIZE PROJECTION traces_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE transactions ADD PROJECTION transactions_count_by_chain (SELECT count(*) as transactions, chain GROUP BY chain);

ALTER TABLE transactions
    MATERIALIZE PROJECTION transactions_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE withdrawals ADD PROJECTION withdrawals_count_by_chain (SELECT count(*) as withdrawals, chain GROUP BY chain);

ALTER TABLE withdrawals
    MATERIALIZE PROJECTION withdrawals_count_by_chain SETTINGS mutations_sync = 1;