fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=migrations");

    std::env::set_var("PROTOC", protobuf_src::protoc());

    tonic_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize)]")
        .type_attribute(".", "#[serde(rename_all = \"camelCase\")]")
        .extern_path(".google.protobuf.Struct", "::prost_wkt_types::Struct")
        .compile_protos(&["proto/api/bria.proto"], &["proto"])?;

    tonic_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize)]")
        .type_attribute(".", "#[serde(rename_all = \"camelCase\")]")
        .compile_protos(&["proto/admin/api.proto"], &["proto"])?;

    Ok(())
}
