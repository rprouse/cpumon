//! Running `cpumon monitor` at logon through Task Scheduler.
//!
//! The task runs as the current user with the interactive logon type ("run only when
//! the user is logged on"), in the user's session, so the monitor can see which of the
//! user's windows stop responding. The command should be the windowless `cpumonw.exe`
//! so that no console window appears. (An S4U task would avoid the window too, but it
//! runs in session 0 and cannot see the user's desktop.) Registration uses the Task
//! Scheduler COM API rather than `schtasks.exe`.

use std::fmt::Write as _;

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::TaskScheduler::{
    IRegisteredTask, ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE,
    TASK_LOGON_INTERACTIVE_TOKEN, TASK_STATE, TASK_STATE_DISABLED, TASK_STATE_QUEUED,
    TASK_STATE_READY, TASK_STATE_RUNNING, TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::BSTR;

use crate::error::Result;

/// Name of the task in the Task Scheduler root folder.
pub const TASK_NAME: &str = "cpumon";

/// What to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    /// `DOMAIN\user` to run as and whose logon starts the task.
    pub user: String,
    /// Full path of the executable (normally the windowless `cpumonw.exe`).
    pub command: String,
    /// Command-line arguments, already quoted.
    pub arguments: String,
    /// Run with the user's full (elevated) token, so system processes can be inspected.
    /// Registering such a task requires an elevated terminal.
    pub elevated: bool,
}

/// State of the installed task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Disabled,
    Queued,
    Ready,
    Running,
    Unknown,
}

impl From<TASK_STATE> for TaskState {
    fn from(state: TASK_STATE) -> Self {
        match state {
            TASK_STATE_DISABLED => Self::Disabled,
            TASK_STATE_QUEUED => Self::Queued,
            TASK_STATE_READY => Self::Ready,
            TASK_STATE_RUNNING => Self::Running,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "disabled",
            Self::Queued => "queued",
            Self::Ready => "ready (not running)",
            Self::Running => "running",
            Self::Unknown => "unknown",
        })
    }
}

/// Escapes text for an XML element.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The task definition in Task Scheduler's XML schema.
pub fn task_xml(spec: &TaskSpec) -> String {
    let user = xml_escape(&spec.user);
    let run_level = if spec.elevated {
        "HighestAvailable"
    } else {
        "LeastPrivilege"
    };
    let mut xml = String::new();
    // `write!` to a String cannot fail.
    let _ = write!(
        xml,
        r#"<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Records processes causing high CPU/GPU usage (cpumon monitor).</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>{run_level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        name = xml_escape(TASK_NAME),
        command = xml_escape(&spec.command),
        arguments = xml_escape(&spec.arguments),
    );
    xml
}

/// Keeps COM initialised on this thread for its lifetime.
struct Com;

impl Com {
    fn init() -> Result<Self> {
        // SAFETY: no pointers; paired with CoUninitialize in Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx in `init`.
        unsafe { CoUninitialize() };
    }
}

fn root_folder() -> Result<ITaskFolder> {
    // SAFETY: COM is initialised by the caller; the CLSID and interface match.
    let service: ITaskService =
        unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }?;
    let empty = VARIANT::default();
    // SAFETY: empty VARIANTs connect to the local machine as the current user.
    unsafe { service.Connect(&empty, &empty, &empty, &empty) }?;
    // SAFETY: valid BSTR argument.
    Ok(unsafe { service.GetFolder(&BSTR::from("\\")) }?)
}

fn find_task(folder: &ITaskFolder) -> Option<IRegisteredTask> {
    // SAFETY: valid BSTR argument. Any failure (usually "not found") means no task.
    unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) }.ok()
}

/// Registers (or replaces) the task. If `start` is set it is also started now rather
/// than at the next logon. Returns the task's state afterwards.
pub fn install(spec: &TaskSpec, start: bool) -> Result<TaskState> {
    let _com = Com::init()?;
    let folder = root_folder()?;
    // Replacing a running task leaves the old process running; stop it first.
    if let Some(existing) = find_task(&folder) {
        // SAFETY: valid task object; 0 = no flags.
        let _ = unsafe { existing.Stop(0) };
    }
    let user = VARIANT::from(BSTR::from(spec.user.as_str()));
    let empty = VARIANT::default();
    // SAFETY: all arguments are valid BSTRs/VARIANTs owned by this frame.
    let task = unsafe {
        folder.RegisterTask(
            &BSTR::from(TASK_NAME),
            &BSTR::from(task_xml(spec)),
            TASK_CREATE_OR_UPDATE.0,
            &user,
            &empty,
            TASK_LOGON_INTERACTIVE_TOKEN,
            &empty,
        )
    }?;
    if start {
        // SAFETY: valid task object; an empty VARIANT means no parameters.
        unsafe { task.Run(&empty) }?;
    }
    // SAFETY: valid task object.
    Ok(unsafe { task.State() }?.into())
}

/// Stops and deletes the task. Returns false if it was not installed.
pub fn uninstall() -> Result<bool> {
    let _com = Com::init()?;
    let folder = root_folder()?;
    let Some(task) = find_task(&folder) else {
        return Ok(false);
    };
    // SAFETY: valid task object; 0 = no flags. It may not be running.
    let _ = unsafe { task.Stop(0) };
    // SAFETY: valid BSTR; 0 = no flags.
    unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) }?;
    Ok(true)
}

/// The installed task's state, or `None` if it is not installed.
pub fn status() -> Result<Option<TaskState>> {
    let _com = Com::init()?;
    let folder = root_folder()?;
    Ok(match find_task(&folder) {
        // SAFETY: valid task object.
        Some(task) => Some(unsafe { task.State() }?.into()),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TaskSpec {
        TaskSpec {
            user: r"PC\me".into(),
            command: r"C:\Tools & Things\cpumon.exe".into(),
            arguments: r#"monitor --db "C:\Users\me\AppData\Local\cpumon\cpumon.db""#.into(),
            elevated: false,
        }
    }

    #[test]
    fn escapes_xml_special_characters() {
        assert_eq!(xml_escape(r#"a&b<c>"d'"#), "a&amp;b&lt;c&gt;&quot;d&apos;");
    }

    #[test]
    fn builds_logon_task_for_user() {
        let xml = task_xml(&spec());
        assert!(xml.contains(r"<UserId>PC\me</UserId>"));
        assert_eq!(xml.matches(r"<UserId>PC\me</UserId>").count(), 2);
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains(r"<Command>C:\Tools &amp; Things\cpumon.exe</Command>"));
        assert!(xml.contains("<Arguments>monitor --db &quot;C:\\Users"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
    }

    #[test]
    fn elevated_task_uses_highest_run_level() {
        let xml = task_xml(&TaskSpec {
            elevated: true,
            ..spec()
        });
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
    }

    #[test]
    fn task_xml_is_well_formed_enough() {
        // Every opened element is closed (a cheap structural check without an XML
        // parser).
        let xml = task_xml(&spec());
        for tag in [
            "Task",
            "Triggers",
            "Principals",
            "Settings",
            "Actions",
            "Exec",
        ] {
            assert_eq!(
                xml.matches(&format!("<{tag}>")).count() + xml.matches(&format!("<{tag} ")).count(),
                xml.matches(&format!("</{tag}>")).count(),
                "{tag}"
            );
        }
    }
}
