extern crate capnpc;

fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/block.capnp")
        .file("schema/contract.capnp")
        .file("schema/log.capnp")
        .file("schema/trace.capnp")
        .file("schema/transaction.capnp")
        .file("schema/withdrawal.capnp")
        .run()
        .expect("capnp compiler failed");
}
