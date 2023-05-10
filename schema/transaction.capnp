@0xbd27528423ddd0f1;

struct AccessListEntry {
    address @0: Text;
    storageKeys @1: List(Text);
}

struct Transaction {
    enum TransactionType {
        legacy @0;
        accessList @1;
        eip1559 @2;
    }

    enum TransactionStatus {
        unkown @0;
        failure @1;
        success @2;
    }

    accessList @0: List(AccessListEntry);
    baseFeePerGas @1: UInt64;
    blockHash @2: Text;
    blockNumber @3: UInt32;
    burned @4: Data;
    chain @5: UInt64;
    contractCreated @6: Text;
    cumulativeGasUsed @7: UInt32;
    effectiveGasPrice @8: Data;
    effectiveTransactionFee @9: Data;
    from @10: Text;
    gas @11: UInt32;
    gasPrice @12: Data;
    gasUsed @13: UInt32;
    hash @14: Text;
    input @15: Text;
    maxFeePerGas @16: Data;
    maxPriorityFeePerGas @17: Data;
    method @18: Text;
    nonce @19: UInt32;
    status @20: TransactionStatus;
    timestamp @21: UInt32;
    to @22: Text;
    transactionIndex @23: UInt16;
    transactionType @24: TransactionType;
    value @25: Data;
}