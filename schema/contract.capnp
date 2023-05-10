@0x9a88fce60a10f819;

struct Contract {
    blockNumber @0: UInt32;
    chain @1: UInt64;
    contractAddress @2: Text;
    creator @3: Text;
    transactionHash @4: Text;
}
