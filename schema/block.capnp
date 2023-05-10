@0xf1c3bac145fa529c;

struct Block {
    baseBlockReward @0: Data;
    baseFeePerGas @1: UInt64;
    burned @2: Data;
    chain @3: UInt64;
    difficulty @4: Data;
    extraData @5: Text;
    gasLimit @6: UInt32;
    gasUsed @7: UInt32;
    hash @8: Text;
    isUncle @9: Bool;
    logsBloom @10: Text;
    miner @11: Text;
    mixHash @12: Text;
    nonce @13: Text;
    number @14: UInt32;
    parentHash @15: Text;
    receiptsRoot @16: Text;
    sha3Uncles @17: Text;
    size @18: UInt32;
    stateRoot @19: Text;
    timestamp @20: UInt32;
    totalDifficulty @21: Data;
    totalFeeReward @22: Data;
    transactions @23: UInt16;
    transactionsRoot @24: Text;
    uncles @25: List(Text);
    uncleRewards @26: Data;
    withdrawalsRoot @27: Text;
}
