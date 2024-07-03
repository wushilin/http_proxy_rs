use std::sync::Arc;
use chrono::{DateTime, Local};
use log::{debug, error, info};
use anycache::CacheAsync;
use regex::Regex;
use serde::{Deserialize, Serialize};
use lazy_static::lazy_static;
use crate::crontab;

use crate::crontab::Crontab;
use crate::request_id::RequestId; // Add this line to import the Serialize and Deserialize traits


lazy_static! {
    pub static ref REGEX_CACHE: CacheAsync<String, Result<Regex, regex::Error>> = CacheAsync::new(string_to_regex_slow);
    pub static ref CRONTAB_CACHE: CacheAsync<String, Result<Crontab, crontab::ParseError>> = CacheAsync::new(string_to_crontab_slow);
}

fn string_to_regex_slow(pattern: &String) -> Result<Regex, regex::Error> {
    Regex::new(pattern)
}

fn string_to_crontab_slow(spec: &String) -> Result<Crontab, crontab::ParseError> {
    Crontab::new(spec)
}

async fn string_to_regex(pattern: &String) -> Arc<Result<Regex, regex::Error>> {
    let result = REGEX_CACHE.get(pattern).await;
    return result;
}

async fn string_to_crontab(pattern: &String) -> Arc<Result<Crontab, crontab::ParseError>> {
    let result = CRONTAB_CACHE.get(pattern).await;
    return result;
}


#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub bind_addr: Option<String>,
    pub bind_port: Option<u16>,
    pub users: Vec<User>,
    pub default_allow: Option<bool>,
    pub allowed: Vec<Rule>,
    pub blocked: Vec<Rule>,
}

impl Config {
    pub fn from_config_file(source: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config_str = std::fs::read_to_string(source)?;
        let config: Config = serde_yaml::from_str(&config_str)?;
        return Ok(config);
    }

    pub fn get_bind_address(&self) -> String {
        self.bind_addr.clone().unwrap_or("0.0.0.0".into())
    }

    pub fn get_bind_port(&self) -> u16 {
        self.bind_port.clone().unwrap_or(3000)
    }

    pub fn get_users(&self) -> &Vec<User> {
        &self.users
    }

    pub fn get_default_allow(&self) -> bool {
        self.default_allow.clone().unwrap_or(false)
    }

    pub fn authenticate_user(&self, user:&str, password:&str) -> bool {
        // no users defined. default allow
        if self.users.is_empty() {
            return true;
        }
        let users = self.get_users();
        let result = users.iter().any(|u| u.username == user && u.password == password);
        return result;
    }

    async fn match_host_against_rule(req_id:&RequestId, host:&str, rule:&Rule, now: &DateTime<Local>) -> bool {
        let host_regex = string_to_regex(&rule.regex).await;
        let crontab = string_to_crontab(&rule.timespec).await;
        let mut time_matched = false;
        let mut regex_matched = false;
        match crontab.as_ref() {
            Ok(cron) => {
                if cron.is_match_now() {
                    info!("{} {} vs {} matched", req_id, now, rule.timespec);
                    time_matched = true;
                } else {
                    info!("{} {} vs {} not matched", req_id, now, rule.timespec);
                }
            },
            Err(cause) => {
                error!("{} invalid crontab: {}, cause {:?}", req_id, rule.timespec, cause);
            }
        }
        match host_regex.as_ref() {
            Ok(re) => {
                let match_result = re.is_match(host);
                if match_result {
                    info!("{} {} vs {} matched", req_id, host, rule.regex);
                    regex_matched = true;
                } else {
                    info!("{} {} vs {} not matched", req_id, host, rule.regex);
                }
            },
            Err(cause) => {
                error!("{} invalid regex: {}, cause {}", req_id, rule.regex, cause);
            },
        }
        if time_matched && regex_matched {
            debug!("{} {} matched", req_id, host);
            return true;
        } else {
            debug!("{} {} not matched", req_id, host);
            return false;
        }
    }

    async fn match_host_against_rules(req_id:&RequestId, host:&str, rules:&Vec<Rule>) -> bool {
        let now: chrono::DateTime<chrono::Local> = chrono::Local::now();

        for rule in rules {
            let result = Self::match_host_against_rule(req_id, host, rule, &now).await;
            if result {
                return true;
            }
        }
        return false;
    }
    pub async fn check_access(&self, req_id:&RequestId, host: &str) -> bool {
        let default_allow = self.get_default_allow();
        if default_allow {
            let rules = &self.blocked;
            let rejected = Self::match_host_against_rules(req_id, host, rules).await;
            if rejected {
                return false;
            } else {
                return true;
            }
        } else {
            let rules = &self.allowed;
            let allowed = Self::match_host_against_rules(req_id, host, rules).await;
            if allowed {
                return true;
            } else {
                return false;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub timespec: String,
    pub regex: String,
}