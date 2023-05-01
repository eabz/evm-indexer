CREATE MATERIALIZED VIEW indexer.blocks_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as blocks
FROM indexer.blocks
GROUP BY chain;

INSERT INTO indexer.blocks_count_by_chain
SELECT
    chain,
    count() as blocks
FROM indexer.blocks
GROUP BY chain;

CREATE MATERIALIZED VIEW blocks_by_chain
ENGINE = MergeTree()
ORDER BY chain
AS
    SELECT number, chain
    FROM blocks
    
INSERT INTO indexer.blocks_by_chain
SELECT
    number,
    chain
FROM indexer.blocks;

CREATE MATERIALIZED VIEW indexer.contracts_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as contracts
FROM indexer.contracts
GROUP BY chain;

INSERT INTO indexer.contracts_count_by_chain
SELECT
    chain,
    count() as contracts
FROM indexer.contracts
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.dex_trades_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as dex_trades
FROM indexer.dex_trades
GROUP BY chain;

INSERT INTO indexer.dex_trades_count_by_chain
SELECT
    chain,
    count() as dex_trades
FROM indexer.dex_trades
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.erc1155_transfers_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as erc1155_transfers
FROM indexer.erc1155_transfers
GROUP BY chain;

INSERT INTO indexer.erc1155_transfers_count_by_chain
SELECT
    chain,
    count() as erc1155_transfers
FROM indexer.erc1155_transfers
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.erc20_transfers_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as erc20_transfers
FROM indexer.erc20_transfers
GROUP BY chain;

INSERT INTO indexer.erc20_transfers_count_by_chain
SELECT
    chain,
    count() as erc20_transfers
FROM indexer.erc20_transfers
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.erc721_transfers_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as erc721_transfers
FROM indexer.erc721_transfers
GROUP BY chain;

INSERT INTO indexer.erc721_transfers_count_by_chain
SELECT
    chain,
    count() as erc721_transfers
FROM indexer.erc721_transfers
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.logs_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as logs
FROM indexer.logs
GROUP BY chain;

INSERT INTO indexer.logs_count_by_chain
SELECT
    chain,
    count() as logs
FROM indexer.logs
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.receipts_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as receipts
FROM indexer.receipts
GROUP BY chain;

INSERT INTO indexer.receipts_count_by_chain
SELECT
    chain,
    count() as receipts
FROM indexer.receipts
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.traces_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as traces
FROM indexer.traces
GROUP BY chain;

INSERT INTO indexer.traces_count_by_chain
SELECT
    chain,
    count() as traces
FROM indexer.traces
GROUP BY chain;

CREATE MATERIALIZED VIEW indexer.transactions_count_by_chain
ENGINE = SummingMergeTree()
ORDER BY chain
AS SELECT
    chain,
    count() as transactions
FROM indexer.transactions
GROUP BY chain;

INSERT INTO indexer.transactions_count_by_chain
SELECT
    chain,
    count() as transactions
FROM indexer.transactions
GROUP BY chain;