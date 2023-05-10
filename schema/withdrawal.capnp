@0xd92273e44ef916dc;

struct Withdrawals {
    address @0 :Text;
    amount @1 :Data;
    blockNumber @2 :UInt32;
    chain @3 :UInt64;
    timestamp @4 :UInt32;
    validatorIndex @5 :UInt32;
    withdrawalIndex @6 :UInt32;
}