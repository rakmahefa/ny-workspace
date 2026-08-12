//! ACP-aware tool executor with real client-side permission requests.
//!
//! Phase 5 keeps the existing `ToolExecutor`/`ToolRegistry` architecture but
//! replaces the previous local oneshot auto-approval with ACP
//! `session/request_permission` requests.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, TextContent, SelectedPermissionOutcome, ToolCall as AcpToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo};

use crate::state::SessionMode;
use super::registry::ToolRegistry;
use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};

#[derive(Debug, Clone)]
pub struct ToolCallMetadata { pub title: String, pub description: String, pub risk: RiskLevel, pub kind: ToolKind }

impl ToolCallMetadata {
    pub fn build(tool_name: &str, arguments: &serde_json::Value) -> Self {
        let kind = Self::tool_kind(tool_name);
        let (title, description, risk) = match tool_name {
            "file_read" => { let p=arg_str(arguments,"path").unwrap_or("<path manquant>"); (format!("Read: {}", truncate(p,60)), format!("Lecture du fichier : {p}"), RiskLevel::Low) }
            "file_write" => { let p=arg_str(arguments,"path").unwrap_or("<path manquant>"); let n=arg_str(arguments,"content").map(str::len).unwrap_or(0); (format!("Write: {}", truncate(p,60)), format!("Écriture du fichier : {p} ({n} octets)"), RiskLevel::Medium) }
            "file_edit" | "replace_in_file" => { let p=arg_str(arguments,"path").unwrap_or("<path manquant>"); (format!("Edit: {}", truncate(p,60)), format!("Modification ciblée du fichier : {p}"), RiskLevel::Medium) }
            "search" | "search_and_read" => { let p=arg_str(arguments,"pattern").unwrap_or("<pattern manquant>"); (format!("Search: {}", truncate(p,60)), format!("Recherche du motif : {p}"), RiskLevel::Low) }
            "shell_exec" => Self::shell_metadata(arguments),
            _ => (tool_name.to_string(), format!("Outil : {tool_name}"), RiskLevel::Medium),
        };
        Self { title, description, risk, kind }
    }

    fn shell_metadata(arguments: &serde_json::Value) -> (String, String, RiskLevel) {
        let command = arg_str(arguments,"command").unwrap_or("<commande manquante>");
        match ShellSandbox::new().analyze_command(command) {
            Ok(a) => (format!("Exec: {}", truncate(command.lines().next().unwrap_or(""),60)), format!("{}\n{} {}", a.summary(), a.risk.emoji(), a.risk.label()), a.risk),
            Err(e) => (format!("Exec: {}", truncate(command,60)), format!("Commande bloquée par la sandbox : {e}"), RiskLevel::Critical),
        }
    }

    fn tool_kind(name: &str) -> ToolKind {
        match name { "file_read" | "search" | "search_and_read" => ToolKind::Read, "file_write" | "file_edit" | "replace_in_file" => ToolKind::Edit, "shell_exec" => ToolKind::Execute, _ => ToolKind::Other }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionKind { Read, Write, Execute, #[allow(dead_code)] Network }

impl PermissionKind { pub fn label(&self)->&'static str { match self { Self::Read=>"read", Self::Write=>"write", Self::Execute=>"execute", Self::Network=>"network" } } }

#[derive(Debug, Clone)]
pub struct PermissionRequest { pub kind: PermissionKind, pub risk: RiskLevel, pub summary: String, pub detail: String, pub tool_name: String, pub warnings: Vec<String> }

impl PermissionRequest {
    pub fn from_tool_call(tool_name:&str,args:&serde_json::Value)->Self {
        let m=ToolCallMetadata::build(tool_name,args);
        let kind=match m.kind { ToolKind::Read=>PermissionKind::Read, ToolKind::Edit=>PermissionKind::Write, ToolKind::Execute=>PermissionKind::Execute, _=>PermissionKind::Execute };
        let mut warnings=Vec::new();
        if m.risk>=RiskLevel::High { warnings.push("Cette opération présente un niveau de risque élevé.".to_string()); }
        if tool_name=="shell_exec" { if let Some(cmd)=arg_str(args,"command") { let a=ShellAnalysis::analyze(cmd); if a.has_env_injection { warnings.push("Injection de variables d'environnement détectée.".to_string()); } if a.has_dangerous_pipe_chain { warnings.push("Chaîne de pipes potentiellement dangereuse détectée.".to_string()); } } }
        let detail=if warnings.is_empty() { format!("{}\nRisque : {} {}",m.description,m.risk.emoji(),m.risk.label()) } else { format!("{}\nRisque : {} {}\nAvertissements :\n{}",m.description,m.risk.emoji(),m.risk.label(),warnings.iter().map(|w|format!("- {w}")).collect::<Vec<_>>().join("\n")) };
        Self { kind, risk:m.risk, summary:m.title, detail, tool_name:tool_name.to_string(), warnings }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult { pub content:String, #[allow(dead_code)] pub is_ok:bool }
impl ToolResult { #[allow(dead_code)] pub fn ok(c:impl Into<String>)->Self{Self{content:c.into(),is_ok:true}} pub fn err(c:impl Into<String>)->Self{Self{content:c.into(),is_ok:false}} }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResult { Allow, Reject, Cancelled, TransportError(String) }

pub struct ToolExecutor<'a> { cx:&'a ConnectionTo<Client>, session_id:&'a SessionId, registry:&'a ToolRegistry, cwd:&'a Path, additional_dirs:&'a [PathBuf], get_mode:&'a (dyn Fn()->SessionMode+Send+Sync) }

impl<'a> ToolExecutor<'a> {
    pub fn new(cx:&'a ConnectionTo<Client>,session_id:&'a SessionId,registry:&'a ToolRegistry,cwd:&'a Path,additional_dirs:&'a [PathBuf],get_mode:&'a (dyn Fn()->SessionMode+Send+Sync))->Self{Self{cx,session_id,registry,cwd,additional_dirs,get_mode}}

    pub async fn execute(&self,tool_name:&str,args:&serde_json::Value)->ToolResult{
        let call_id=ToolCallId::from(format!("call_{}",uuid::Uuid::new_v4().simple()));
        let meta=ToolCallMetadata::build(tool_name,args); let gated=self.needs_permission(meta.kind,meta.risk);
        self.emit_tool_call(&call_id,&meta,if gated{ToolCallStatus::Pending}else{ToolCallStatus::InProgress},args);
        if gated { let req=PermissionRequest::from_tool_call(tool_name,args); match self.request_permission(&call_id,&req).await { PermissionResult::Allow=>self.emit_status(&call_id,ToolCallStatus::InProgress), PermissionResult::Reject|PermissionResult::Cancelled=>{let m=format!("Permission denied for {} ({})",req.tool_name,req.summary); self.emit_result(&call_id,ToolCallStatus::Failed,&m); return ToolResult::err(m)}, PermissionResult::TransportError(e)=>{let m=format!("Permission request failed: {e}"); self.emit_result(&call_id,ToolCallStatus::Failed,&m); return ToolResult::err(m)} } }
        match self.registry.call_async(tool_name,args,self.cwd,self.additional_dirs).await { Some(r)=>{let st=if r.is_ok(){ToolCallStatus::Completed}else{ToolCallStatus::Failed}; let t=r.to_history_text(); self.emit_result(&call_id,st,&t); ToolResult{content:t,is_ok:r.is_ok()}}, None=>{let m=format!("Unknown tool: {tool_name}"); self.emit_result(&call_id,ToolCallStatus::Failed,&m); ToolResult::err(m)} }
    }

    fn needs_permission(&self,kind:ToolKind,risk:RiskLevel)->bool{
        match (self.get_mode)() { SessionMode::BypassPermissions=>false, SessionMode::AcceptEdits=>match kind { ToolKind::Read=>risk>=RiskLevel::High, ToolKind::Edit|ToolKind::Execute=>risk>=RiskLevel::High, _=>risk>=RiskLevel::High }, SessionMode::Default=>match kind { ToolKind::Edit|ToolKind::Execute=>true, ToolKind::Read=>risk>=RiskLevel::High, _=>risk>=RiskLevel::Medium }, _=>match kind { ToolKind::Edit|ToolKind::Execute=>true, _=>risk>=RiskLevel::Medium } }
    }

    async fn request_permission(&self,call_id:&ToolCallId,req:&PermissionRequest)->PermissionResult{
        let options=vec![PermissionOption::new("allow_once","Allow once",PermissionOptionKind::AllowOnce),PermissionOption::new("allow_always","Allow always",PermissionOptionKind::AllowAlways),PermissionOption::new("reject_once","Reject",PermissionOptionKind::RejectOnce),PermissionOption::new("reject_always","Reject always",PermissionOptionKind::RejectAlways)];
        let update=ToolCallUpdate::new(call_id.clone(),ToolCallUpdateFields::new().status(ToolCallStatus::Pending).content(vec![ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(req.detail.clone()))))]));
        let rpc=RequestPermissionRequest::new(self.session_id.clone(),update,options);
        match self.cx.send_request(rpc).block_task().await { Ok(r)=>self.map_response(r), Err(e)=>PermissionResult::TransportError(e.to_string()) }
    }

    fn map_response(&self,response:RequestPermissionResponse)->PermissionResult{
        match response.outcome { RequestPermissionOutcome::Cancelled=>PermissionResult::Cancelled, RequestPermissionOutcome::Selected(s)=>match s.option_id.to_string().as_str(){"allow_once"|"allow_always"=>PermissionResult::Allow,"reject_once"|"reject_always"=>PermissionResult::Reject,_=>PermissionResult::Reject}, _=>PermissionResult::Cancelled }
    }

    fn emit_tool_call(&self,id:&ToolCallId,meta:&ToolCallMetadata,status:ToolCallStatus,input:&serde_json::Value){ let _=self.cx.send_notification(SessionNotification::new(self.session_id.clone(),SessionUpdate::ToolCall(AcpToolCall::new(id.clone(),format!("{} {}",meta.risk.emoji(),meta.title)).kind(meta.kind).status(status).raw_input(input.clone())))); }
    fn emit_status(&self,id:&ToolCallId,status:ToolCallStatus){self.emit_update(id,ToolCallUpdateFields::new().status(status));}
    fn emit_result(&self,id:&ToolCallId,status:ToolCallStatus,text:&str){self.emit_update(id,ToolCallUpdateFields::new().status(status).content(vec![ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text.to_string()))))]));}
    fn emit_update(&self,id:&ToolCallId,fields:ToolCallUpdateFields){let _=self.cx.send_notification(SessionNotification::new(self.session_id.clone(),SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id.clone(),fields))));}
}

pub fn safe_session_update(cx:&ConnectionTo<Client>,sid:&SessionId,update:SessionUpdate){let _=cx.send_notification(SessionNotification::new(sid.clone(),update));}
pub fn emit_error_chunk(cx:&ConnectionTo<Client>,sid:&SessionId,message_id:&agent_client_protocol::schema::v1::MessageId,error:&str){safe_session_update(cx,sid,SessionUpdate::AgentMessageChunk(agent_client_protocol::schema::v1::ContentChunk::new(ContentBlock::Text(TextContent::new(format!("\n\n[error] {error}")))).message_id(message_id.clone())));}
#[allow(dead_code)]
pub fn map_stop_reason(g:Option<&str>)->agent_client_protocol::schema::v1::StopReason{match g{Some("length")|Some("max_tokens")=>agent_client_protocol::schema::v1::StopReason::MaxTokens,Some("content_filter")|Some("safety")|Some("block_reason")=>agent_client_protocol::schema::v1::StopReason::Refusal,_=>agent_client_protocol::schema::v1::StopReason::EndTurn}}
fn arg_str<'a>(args:&'a serde_json::Value,name:&str)->Option<&'a str>{args.get(name).and_then(serde_json::Value::as_str).filter(|v|!v.is_empty())}
fn truncate(v:&str,max:usize)->String{if v.chars().count()<=max{return v.to_string()} let mut s:String=v.chars().take(max.saturating_sub(3)).collect();s.push_str("...");s}

#[cfg(test)]
mod tests { use super::*; #[test] fn permission_labels(){assert_eq!(PermissionKind::Read.label(),"read");assert_eq!(PermissionKind::Write.label(),"write");assert_eq!(PermissionKind::Execute.label(),"execute");} #[test] fn builtin_kind_mapping(){assert_eq!(ToolCallMetadata::build("file_read",&serde_json::json!({"path":"a.rs"})).kind,ToolKind::Read);assert_eq!(ToolCallMetadata::build("file_write",&serde_json::json!({"path":"a.rs","content":"x"})).kind,ToolKind::Edit);assert_eq!(ToolCallMetadata::build("shell_exec",&serde_json::json!({"command":"git status"})).kind,ToolKind::Execute);} }
