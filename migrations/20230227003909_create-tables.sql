CREATE TYPE "BLOCK_STATUS" AS ENUM (
  'unfinalized',
  'secure',
  'finalized'
);

CREATE TYPE "TRANSACTION_STATUS" AS ENUM (
  'reverted',
  'succeed',
  'pending'
);

CREATE TYPE "TRADE_TYPE" AS ENUM (
  'buy',
  'sell'
);

CREATE TABLE "blocks" (
  base_fee_per_gas DECIMAL,
  "chain" BIGINT NOT NULL,
  "difficulty" VARCHAR(66) NOT NULL,
  "extra_data" BYTEA NOT NULL,
  "gas_limit" BIGINT NOT NULL,
  "gas_used" BIGINT NOT NULL,
  "hash" VARCHAR(66) UNIQUE PRIMARY KEY,
  "logs_bloom" BYTEA NOT NULL,
  "miner" VARCHAR(42) NOT NULL,
  "mix_hash" VARCHAR(66) NOT NULL,
  "nonce" VARCHAR(42) NOT NULL,
  "number" BIGINT NOT NULL,
  "parent_hash" VARCHAR(66) NOT NULL,
  "receipts_root" VARCHAR(66) NOT NULL,
  "sha3_uncles" VARCHAR(66) NOT NULL,
  "size" INT NOT NULL,
  "state_root" VARCHAR(66) NOT NULL,
  "status" BLOCK_STATUS,
  "timestamp" TIMESTAMP NOT NULL,
  "total_difficulty" VARCHAR(66) NOT NULL,
  "transactions" INT NOT NULL,
  "transactions_root" VARCHAR(66) NOT NULL,
  "uncles" VARCHAR[](66) NOT NULL
);

CREATE TABLE "transactions" (
  "block_hash" VARCHAR(66) NOT NULL,
  "block_number" BIGINT NOT NULL,
  "chain" BIGINT NOT NULL,
  "from_address" VARCHAR(42) NOT NULL,
  "gas_limit" BIGINT NOT NULL,
  "gas_price" BIGINT NOT NULL,
  "hash" VARCHAR(66) UNIQUE PRIMARY KEY,
  "input" BYTEA NOT NULL,
  "max_fee_per_gas" BIGINT,
  "max_priority_fee_per_gas" BIGINT,
  "method" VARCHAR(10) NOT NULL,
  "nonce" INT NOT NULL,
  "timestamp" TIMESTAMP NOT NULL,
  "to_address" VARCHAR(42),
  "transaction_index" SMALLINT NOT NULL,
  "transaction_type" SMALLINT NOT NULL,
  "value" DECIMAL NOT NULL
);

CREATE TABLE "methods" (
  "name" TEXT NOT NULL,
  "method" VARCHAR(10) UNIQUE PRIMARY KEY
);

CREATE TABLE "receipts" (
  "contract_address" VARCHAR(42),
  "cumulative_gas_used" BIGINT NOT NULL,
  "effective_gas_price" BIGINT,
  "gas_used" BIGINT NOT NULL,
  "hash" VARCHAR(66) UNIQUE PRIMARY KEY,
  "status" TRANSACTION_STATUS
);

CREATE TABLE "contracts" (
  "contract_address" VARCHAR(42) NOT NULL,
  "chain" BIGINT NOT NULL,
  "creator" VARCHAR(42) NOT NULL,
  "hash" VARCHAR(66) NOT NULL,
  PRIMARY KEY ("contract_address", "chain")
);

CREATE TABLE "contract_metadata" (
  "abi" JSONB NOT NULL,
  "chain" BIGINT NOT NULL,
  "contract_address" VARCHAR(42) NOT NULL,
  "name" TEXT NOT NULL,
  PRIMARY KEY ("contract_address", "chain")
);

CREATE TABLE "logs" (
  "address" VARCHAR(42) NOT NULL,
  "chain" BIGINT NOT NULL,
  "data" BYTEA NOT NULL,
  "hash" VARCHAR(66) NOT NULL,
  "log_index" INT NOT NULL,
  "log_type" SMALLINT,
  "removed" BOOLEAN NOT NULL,
  "topics" VARCHAR[](66) NOT NULL,
  "transaction_log_index" INT,
  PRIMARY KEY ("hash", "transaction_log_index")
);

CREATE TABLE "erc20_transfers" (
  "chain" BIGINT NOT NULL,
  "from_address" VARCHAR(42) NOT NULL,
  "hash" VARCHAR(66) NOT NULL,
  "log_index" INT NOT NULL,
  "to_address" VARCHAR(42) NOT NULL,
  "token" VARCHAR(42) NOT NULL,
  "transaction_log_index" INT NOT NULL,
  "amount" DECIMAL NOT NULL,
  "timestamp" TIMESTAMP NOT NULL,
  PRIMARY KEY ("hash", "transaction_log_index")
);

CREATE TABLE "erc721_transfers" (
  "chain" BIGINT NOT NULL,
  "from_address" VARCHAR(42) NOT NULL,
  "hash" VARCHAR(66) NOT NULL,
  "log_index" INT NOT NULL,
  "to_address" VARCHAR(42) NOT NULL,
  "token" VARCHAR(42) NOT NULL,
  "transaction_log_index" INT NOT NULL,
  "id" BIGINT NOT NULL,
  "timestamp" TIMESTAMP NOT NULL,
  PRIMARY KEY ("hash", "transaction_log_index")
);

CREATE TABLE "erc1155_transfers" (
  "chain" BIGINT NOT NULL,
  "from_address" VARCHAR(42) NOT NULL,
  "hash" VARCHAR(66) NOT NULL,
  "log_index" INT NOT NULL,
  "to_address" VARCHAR(42) NOT NULL,
  "token" VARCHAR(42) NOT NULL,
  "transaction_log_index" INT NOT NULL,
  "id" BIGINT NOT NULL,
  "amount" DECIMAL NOT NULL,
  "timestamp" TIMESTAMP NOT NULL,
  PRIMARY KEY ("hash", "transaction_log_index")
);

CREATE TABLE "dex_trades" (
  "chain" BIGINT NOT NULL,
  "maker" VARCHAR(42) NOT NULL,
  "hash" VARCHAR(66) NOT NULL,
  "log_index" INT NOT NULL,
  "receiver" VARCHAR(42) NOT NULL,
  "token_in" VARCHAR(42) NOT NULL,
  "token_amount_in" DECIMAL NOT NULL,
  "token_out" VARCHAR(42) NOT NULL,
  "token_amount_out" DECIMAL NOT NULL,
  "usd_value" DECIMAL NOT NULL,
  "swap_rate" DECIMAL NOT NULL,
  "transaction_log_index" INT NOT NULL,
  "timestamp" TIMESTAMP NOT NULL,
  "trade_type" TRADE_TYPE NOT NULL,
  PRIMARY KEY ("hash", "transaction_log_index")
);

CREATE TABLE "token_details" (
  "chain" BIGINT NOT NULL,
  "token" VARCHAR(42) NOT NULL,
  "name" TEXT NOT NULL,
  "symbol" TEXT NOT NULL,
  "decimals" SMALLINT,
  PRIMARY KEY ("token", "chain")
);

ALTER TABLE "transactions" ADD FOREIGN KEY ("block_hash") REFERENCES "blocks" ("hash");

ALTER TABLE "methods" ADD FOREIGN KEY ("method") REFERENCES "transactions" ("method");

ALTER TABLE "receipts" ADD FOREIGN KEY ("hash") REFERENCES "transactions" ("hash");

ALTER TABLE "contracts" ADD FOREIGN KEY ("hash") REFERENCES "transactions" ("hash");

ALTER TABLE "contract_metadata" ADD FOREIGN KEY ("contract_address", "chain") REFERENCES "contracts" ("contract_address", "chain");

ALTER TABLE "logs" ADD FOREIGN KEY ("hash") REFERENCES "transactions" ("hash");

ALTER TABLE "erc20_transfers" ADD FOREIGN KEY ("hash") REFERENCES "transactions" ("hash");

ALTER TABLE "erc721_transfers" ADD FOREIGN KEY ("hash") REFERENCES "transactions" ("hash");

ALTER TABLE "erc1155_transfers" ADD FOREIGN KEY ("hash") REFERENCES "transactions" ("hash");

ALTER TABLE "dex_trades" ADD FOREIGN KEY ("hash") REFERENCES "transactions" ("hash");

ALTER TABLE "token_details" ADD FOREIGN KEY ("token", "chain") REFERENCES "erc20_transfers" ("token", "chain");

ALTER TABLE "token_details" ADD FOREIGN KEY ("token", "chain") REFERENCES "erc721_transfers" ("token", "chain");

ALTER TABLE "token_details" ADD FOREIGN KEY ("token", "chain") REFERENCES "erc1155_transfers" ("token", "chain");

ALTER TABLE "token_details" ADD FOREIGN KEY ("token", "chain") REFERENCES "dex_trades" ("token_in", "chain");

ALTER TABLE "token_details" ADD FOREIGN KEY ("token", "chain") REFERENCES "dex_trades" ("token_out", "chain");
