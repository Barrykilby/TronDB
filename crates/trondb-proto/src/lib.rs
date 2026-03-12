pub mod pb {
    tonic::include_proto!("trondb");
}

pub mod convert_plan;
pub mod convert_result;
pub mod convert_wal;
pub mod convert_health;
