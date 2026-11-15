//! 读取飞书通讯录可见范围、部门及成员；每次只处理一页。

use std::collections::{HashSet, VecDeque};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::service::DirectoryError;
use crate::feishu::ResolvedConfig;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_WORK: usize = 100_000;

#[derive(Deserialize)]
pub(super) struct DirectoryMember {
    pub open_id: Option<String>,
    pub union_id: Option<String>,
    pub name: Option<String>,
    pub en_name: Option<String>,
    pub email: Option<String>,
    pub avatar: Option<Avatar>,
}

#[derive(Deserialize)]
pub(super) struct Avatar {
    pub avatar_240: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Scope,
    Departments,
    Members,
    User,
    GroupUsers,
    GroupDepartments,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Work {
    kind: Kind,
    id: String,
    token: Option<String>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    code: i64,
    data: Option<T>,
}
#[derive(Deserialize)]
struct Token {
    code: i64,
    tenant_access_token: Option<String>,
}
#[derive(Deserialize)]
struct Scope {
    #[serde(default)]
    user_ids: Vec<String>,
    #[serde(default)]
    department_ids: Vec<String>,
    #[serde(default)]
    group_ids: Vec<String>,
    has_more: bool,
    page_token: Option<String>,
}
#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    has_more: bool,
    page_token: Option<String>,
}
#[derive(Deserialize)]
struct Department {
    open_department_id: String,
}
#[derive(Deserialize)]
struct MemberBody {
    user: DirectoryMember,
}
#[derive(Deserialize)]
struct GroupPage {
    memberlist: Vec<GroupMember>,
    has_more: bool,
    page_token: Option<String>,
}
#[derive(Deserialize)]
struct GroupMember {
    member_id: String,
    member_id_type: Option<String>,
    member_type: String,
}

pub(super) struct DirectoryReader {
    http: reqwest::Client,
    cfg: ResolvedConfig,
    token: Option<String>,
    queue: VecDeque<Work>,
    scheduled: HashSet<Work>,
}

pub(super) fn validate_base(base: &str) -> Result<(), DirectoryError> {
    let url = reqwest::Url::parse(base).map_err(|_| DirectoryError::ConfigUnavailable)?;
    let loopback = url
        .host_str()
        .is_some_and(|host| host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()));
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
    {
        return Err(DirectoryError::ConfigUnavailable);
    }
    Ok(())
}

impl DirectoryReader {
    pub(super) fn new(http: reqwest::Client, cfg: ResolvedConfig) -> Self {
        let work = Work {
            kind: Kind::Scope,
            id: String::new(),
            token: None,
        };
        Self {
            http,
            cfg,
            token: None,
            queue: VecDeque::from([work.clone()]),
            scheduled: HashSet::from([work]),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn enqueue(&mut self, work: Work) -> Result<(), DirectoryError> {
        if self.scheduled.contains(&work) {
            return Ok(());
        }
        if self.scheduled.len() >= MAX_WORK {
            return Err(DirectoryError::Failed);
        }
        self.scheduled.insert(work.clone());
        self.queue.push_back(work);
        Ok(())
    }

    fn more(&mut self, work: &Work, has_more: bool, token: Option<String>) -> Result<(), DirectoryError> {
        if !has_more {
            return Ok(());
        }
        let token = token
            .filter(|token| !token.is_empty() && token.len() <= 4096)
            .ok_or(DirectoryError::Failed)?;
        let next = Work {
            token: Some(token),
            ..work.clone()
        };
        if self.scheduled.contains(&next) {
            return Err(DirectoryError::Failed);
        }
        self.enqueue(next)
    }

    fn member(&mut self, open_id: String) -> Result<(), DirectoryError> {
        self.enqueue(Work {
            kind: Kind::User,
            id: open_id,
            token: None,
        })
    }

    fn department(&mut self, id: String) -> Result<(), DirectoryError> {
        self.enqueue(Work {
            kind: Kind::Members,
            id: id.clone(),
            token: None,
        })?;
        self.enqueue(Work {
            kind: Kind::Departments,
            id,
            token: None,
        })
    }

    async fn get<T: DeserializeOwned>(&self, work: &Work) -> Result<T, DirectoryError> {
        let path = match work.kind {
            Kind::Scope => "/open-apis/contact/v3/scopes",
            Kind::Departments => "/open-apis/contact/v3/departments",
            Kind::Members => "/open-apis/contact/v3/users/find_by_department",
            Kind::User => "/open-apis/contact/v3/users",
            Kind::GroupUsers | Kind::GroupDepartments => "/open-apis/contact/v3/group",
        };
        let mut url = reqwest::Url::parse(&format!("{}{path}", self.cfg.api_base.trim_end_matches('/')))
            .map_err(|_| DirectoryError::ConfigUnavailable)?;
        if matches!(work.kind, Kind::User | Kind::GroupUsers | Kind::GroupDepartments) {
            if work.id.is_empty() || work.id.len() > 256 {
                return Err(DirectoryError::Failed);
            }
            let mut segments = url.path_segments_mut().map_err(|_| DirectoryError::ConfigUnavailable)?;
            segments.push(&work.id);
            if matches!(work.kind, Kind::GroupUsers | Kind::GroupDepartments) {
                segments.extend(["member", "simplelist"]);
            }
        }
        {
            let mut query = url.query_pairs_mut();
            if matches!(work.kind, Kind::GroupUsers | Kind::GroupDepartments) {
                let member_type = if work.kind == Kind::GroupUsers {
                    "user"
                } else {
                    "department"
                };
                query
                    .append_pair("member_id_type", "open_id")
                    .append_pair("member_type", member_type);
            } else {
                query
                    .append_pair("user_id_type", "open_id")
                    .append_pair("department_id_type", "open_department_id");
            }
            if work.kind != Kind::User {
                query.append_pair("page_size", "50");
            }
            if let Some(token) = &work.token {
                query.append_pair("page_token", token);
            }
            if work.kind == Kind::Members {
                query.append_pair("department_id", &work.id);
            }
            if work.kind == Kind::Departments {
                query
                    .append_pair("parent_department_id", &work.id)
                    .append_pair("fetch_child", "false");
            }
        }
        let response = self
            .http
            .get(url)
            .bearer_auth(self.token.as_deref().ok_or(DirectoryError::Failed)?)
            .send()
            .await
            .map_err(http_error)?;
        // 先检查错误码，避免错误响应中的空data被当作成功DTO解析失败。
        let response: Envelope<serde_json::Value> = read_json(response).await?;
        platform_code(response.code)?;
        serde_json::from_value(response.data.ok_or(DirectoryError::Failed)?).map_err(|_| DirectoryError::Failed)
    }

    pub(super) async fn next(&mut self) -> Result<Vec<DirectoryMember>, DirectoryError> {
        let Some(work) = self.queue.pop_front() else {
            return Ok(Vec::new());
        };
        if self.token.is_none() {
            let response = self
                .http
                .post(format!(
                    "{}/open-apis/auth/v3/tenant_access_token/internal",
                    self.cfg.api_base.trim_end_matches('/')
                ))
                .json(&serde_json::json!({"app_id":self.cfg.app_id,"app_secret":self.cfg.app_secret}))
                .send()
                .await
                .map_err(http_error)?;
            let token: Token = read_json(response).await?;
            platform_code(token.code)?;
            self.token = Some(
                token
                    .tenant_access_token
                    .filter(|token| !token.is_empty())
                    .ok_or(DirectoryError::Failed)?,
            );
        }
        match work.kind {
            Kind::Scope => {
                let scope: Scope = self.get(&work).await?;
                for user in scope.user_ids {
                    self.member(user)?;
                }
                for department in scope.department_ids {
                    self.department(department)?;
                }
                for group in scope.group_ids {
                    self.enqueue(Work {
                        kind: Kind::GroupUsers,
                        id: group.clone(),
                        token: None,
                    })?;
                    self.enqueue(Work {
                        kind: Kind::GroupDepartments,
                        id: group,
                        token: None,
                    })?;
                }
                self.more(&work, scope.has_more, scope.page_token)?;
                Ok(Vec::new())
            }
            Kind::Departments => {
                let page: Page<Department> = self.get(&work).await?;
                for department in page.items {
                    self.department(department.open_department_id)?;
                }
                self.more(&work, page.has_more, page.page_token)?;
                Ok(Vec::new())
            }
            Kind::Members => {
                let page: Page<DirectoryMember> = self.get(&work).await?;
                self.more(&work, page.has_more, page.page_token)?;
                Ok(page.items)
            }
            Kind::User => {
                let body: MemberBody = self.get(&work).await?;
                if body.user.open_id.as_deref() != Some(work.id.as_str()) {
                    return Err(DirectoryError::Failed);
                }
                Ok(vec![body.user])
            }
            Kind::GroupUsers | Kind::GroupDepartments => {
                let page: GroupPage = self.get(&work).await?;
                let expected_type = if work.kind == Kind::GroupUsers {
                    "user"
                } else {
                    "department"
                };
                for member in page.memberlist {
                    if member.member_type != expected_type
                        || member.member_id_type.as_deref().is_some_and(|kind| {
                            kind != "open_id" && !(work.kind == Kind::GroupDepartments && kind == "open_department_id")
                        })
                    {
                        return Err(DirectoryError::Failed);
                    }
                    if work.kind == Kind::GroupUsers {
                        self.member(member.member_id)?;
                    } else {
                        self.department(member.member_id)?;
                    }
                }
                self.more(&work, page.has_more, page.page_token)?;
                Ok(Vec::new())
            }
        }
    }
}

fn platform_code(code: i64) -> Result<(), DirectoryError> {
    match code {
        0 => Ok(()),
        99991671 | 99991672 => Err(DirectoryError::PermissionDenied),
        _ => Err(DirectoryError::Failed),
    }
}

fn http_error(error: reqwest::Error) -> DirectoryError {
    if error.is_timeout() {
        DirectoryError::Timeout
    } else {
        DirectoryError::Failed
    }
}

async fn read_json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, DirectoryError> {
    if response.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(DirectoryError::PermissionDenied);
    }
    if !response.status().is_success() {
        return Err(DirectoryError::Failed);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(DirectoryError::Failed);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| DirectoryError::Failed)
}
