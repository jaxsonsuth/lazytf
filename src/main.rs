use std::{collections::HashMap, error::Error, fs::File, io::Read, path::PathBuf};

use serde::Deserialize;
use serde_yaml::from_str;

const config_path: &str = "../config.yaml";

struct AppState {
    accounts: HashMap<String, Account>,
    current_account: Option<String>,
    current_workspace: Option<String>,
}

impl AppState {
    fn new(accounts: HashMap<String, Account>) -> Self {
        Self {
            accounts,
            current_account: None,
            current_workspace: None,
        }
    }

    fn init() -> Result<AppState, Box<dyn Error>> {
        let mut file = File::open(config_path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let config: Config = from_str(&contents).unwrap();

        let mut accounts = HashMap::new();

        for (account_name, conf) in config.accounts.iter() {
            let account: Account = Account {
                path: conf.composition_path,
                workspaces: None,
                aws_profile: conf.aws_profile,
                region: conf.region,
                auth: AuthStatus::Unknown,
            };

            accounts.insert(account_name, account);
        }

        Ok(AppState::new(accounts))
    }
}

struct Account {
    path: PathBuf,
    workspaces: Option<Vec<String>>,
    aws_profile: String,
    region: String,
    auth: AuthStatus,
}

enum AuthStatus {
    Unknown,
    Checking,
    Authenticated,
    Failed,
}

#[derive(Deserialize)]
struct Config {
    accounts: HashMap<String, AccountConfig>,
}

#[derive(Deserialize)]
struct AccountConfig {
    aws_profile: String,
    region: String,
    composition_path: PathBuf,
}

fn main() {}
