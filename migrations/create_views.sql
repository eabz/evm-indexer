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

ALTER TABLE erc20_transfers ADD PROJECTION erc20_transfers_count_by_chain (SELECT count(*) as erc20_transfers, chain GROUP BY chain);

ALTER TABLE erc20_transfers 
    MATERIALIZE PROJECTION erc20_transfers_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE erc721_transfers ADD PROJECTION erc721_transfers_count_by_chain (SELECT count(*) as erc721_transfers, chain GROUP BY chain);

ALTER TABLE erc721_transfers 
    MATERIALIZE PROJECTION erc721_transfers_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE erc1155_transfers ADD PROJECTION erc1155_transfers_count_by_chain (SELECT count(*) as erc1155_transfers, chain GROUP BY chain);

ALTER TABLE erc1155_transfers 
    MATERIALIZE PROJECTION erc1155_transfers_count_by_chain SETTINGS mutations_sync = 1;

ALTER TABLE dex_trades ADD PROJECTION dex_trades_count_by_chain (SELECT count(*) as dex_trades, chain GROUP BY chain);

ALTER TABLE dex_trades 
    MATERIALIZE PROJECTION dex_trades_count_by_chain SETTINGS mutations_sync = 1;


