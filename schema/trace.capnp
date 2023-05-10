@0xbbe61ed52931dade;

struct Trace {
    enum TraceType {
        call @0;
        create @1;
        suicide @2;
        reward @3;
    }

    enum CallType {
        none @0;
        call @1;
        callcode @2;
        delegatecall @3;
        staticcall @4;
    }

    enum RewardType {
        block @0;
        uncle @1;
        emptyStep @2;
        external @3;
    }

    actionType @0: TraceType;
    address @1: Text;
    author @2: Text;
    balance @3: Data;
    blockHash @4: Text;
    blockNumber @5: UInt32;
    callType @6: CallType;
    chain @7: UInt64;
    code @8: Text;
    error @9: Text;
    from @10: Text;
    gas @11: UInt32;
    gasUsed @12: UInt32;
    init @13: Text;
    input @14: Text;
    output @15: Text;
    refundAddress @16: Text;
    rewardType @17: RewardType;
    subtraces @18: UInt16;
    to @19: Text;
    traceAddress @20: List(UInt16);
    transactionHash @21: Text;
    transactionPosition @22: UInt16;
    value @23: Data;
}