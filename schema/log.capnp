@0xd71a0a3763a8bde1;

struct Log {
    enum TokenTransferType {
        erc20 @0;
        erc721 @1;
        erc1155 @2;
    }

    address @0: Text;
    blockNumber @1: UInt32;
    chain @2: UInt64;
    data @3: Text;
    dexTradeMaker @4: Text;
    dexTradePair @5: Text;
    dexTradeReceiver @6: Text;
    dexTradeToken0Amount @7: Data;
    dexTradeToken1Amount @8: Data;
    logIndex @9: UInt16;
    logType @10: Text;
    removed @11: Bool;
    timestamp @12: UInt32;
    tokenTransferAmount @13: Data;
    tokenTransferAmounts @14: List(Data);
    tokenTransferFrom @15: Text;
    tokenTransferId @16: Data;
    tokenTransferIds @17: List(Data);
    tokenTransferOperator @18: Text;
    tokenTransferTo @19: Text;
    tokenTransferTokenAddress @20: Text;
    tokenTransferType @21: TokenTransferType;
    topic0 @22: Text;
    topic1 @23: Text;
    topic2 @24: Text;
    topic3 @25: Text;
    transactionHash @26: Text;
    transactionLogIndex @27: UInt16;
}