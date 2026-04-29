pub mod api;
pub mod config;
pub mod db;

pub struct AppState {
    pub pool: db::Db,
    pub config: config::Config,
}
