//! 对**本机已安装的真实运行时**跑的 live 测试（`#[ignore]`，名字以 `remote_live_` 开头）。
//!
//! 全部默认 `#[ignore]`：它们会起一个真的 `dsh --profile web` 进程、要一个真的
//! `node`、并且在启动的头几秒里占 CPU。离线套件（`cargo test -p saiku-dsh`）
//! 不该被这些东西拖住，也不该在没装运行时的机器上红。
//!
//! ## 这一层守什么
//!
//! 离线套件里假服务端验的是**我们自己的分发代码**；真实内核验的是**我们对 0.2.1
//! 的理解**。两件事都会错，而错法不同 —— 假服务端的形状是我们自己写的，
//! 它错了只会「我们跟自己对上了」。
//!
//! 所以这三条测试的形状是：起真内核 → 换票 → 调一个**只读**方法 / 开一条
//! **不需要已有会话**的流 → 关掉 → 确认进程真的退了。
//!
//! **不发任何 prompt**（那会消耗模型配额），也不需要密钥：`session/list` 与
//! `session/control` 都不碰模型。
//!
//! ## `DSH_HOME` 是临时目录
//!
//! 参 AGENTS.md P15：`session/follow` 会 **promote** 一个冷会话（把它接管成活动
//! 会话）。指到 DSH Desktop 的真实 home 就等于让第二个内核去碰它 —— 所以这里
//! 每个测试一个 `tempfile::TempDir`，跑完连目录一起删。
//!
//! ## 跑法
//!
//! ```text
//! cargo test -p saiku-dsh --offline -- --ignored remote_live_
//! ```
//!
//! 运行时不在默认位置时（换机器、CI）：`SAIKU_DSH_RUNTIME_DIR=<目录>`，
//! 或者照 `crates/saiku-cli/src/commands.rs` 的 `cmd_runtime` 用 `--config`
//! 指一份配置 —— 这里只认环境变量，因为测试不该去解析用户的配置。

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::remote::events::Forwarded;
use crate::remote::host::{find_node, WebKernel, WebKernelOptions};
use crate::remote::{RemoteFrame, RemoteMux, RemoteRpc, Secret, SessionClient};

/// 起一个内核给一条测试用。
///
/// 内核冷启动要装完整棵插件树，实测几十秒 —— 离线套件的默认超时扛不住，
/// 所以这里显式给一个上限，而不是靠 `WebKernelOptions` 的默认值碰运气。
const LIVE_STARTUP_TIMEOUT: Duration = Duration::from_secs(180);

/// 一条 live 测试的公共前半段：找到运行时 → 建临时 home → 起内核。
///
/// 返回 `None` 表示「这台机器上没装我们要的那个运行时」—— 那时测试打印原因并
/// **跳过**，而不是失败。理由：`--ignored` 是人工挑着跑的，把「没装」报成
/// 「协议对不上」会把一个环境问题变成一条假的协议结论。
async fn start_kernel(test_name: &str) -> Option<(WebKernel, tempfile::TempDir)> {
    let layout = match runtime_layout() {
        Ok(layout) => layout,
        Err(reason) => {
            eprintln!("跳过 {test_name}：{reason}");
            return None;
        }
    };
    let node = match find_node(&layout.dir) {
        Some(node) => node,
        None => {
            eprintln!(
                "跳过 {test_name}：找不到 `node`（既不在 {} 里也不在 PATH 上）",
                layout.dir.display()
            );
            return None;
        }
    };
    let home = tempfile::Builder::new()
        .prefix("saiku-remote-live-")
        .tempdir()
        .expect("建临时 DSH_HOME");

    let mut options = WebKernelOptions::new(node, layout.entry(), home.path());
    options.startup_timeout = LIVE_STARTUP_TIMEOUT;
    let kernel = match WebKernel::start(options).await {
        Ok(kernel) => kernel,
        Err(error) => panic!("起 web 内核失败：{error}"),
    };
    Some((kernel, home))
}

/// 本机那个钉住的运行时。找不到就给出**为什么**（而不是一个 bool）。
fn runtime_layout() -> Result<crate::runtime::RuntimeLayout, String> {
    use crate::runtime::RuntimeLayout;

    // 版本可以覆盖：`SAIKU_DSH_TEST_VERSION=0.2.2-rc.2` + `SAIKU_DSH_RUNTIME_DIR`
    // 就能拿同一套 live 断言体检候选内核（见 `docs/DSH_CONTRACT.md`）。
    let version = std::env::var("SAIKU_DSH_TEST_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| saiku_core::userconfig::PINNED_DSH_VERSION.to_string());
    let dir: PathBuf = match std::env::var_os("SAIKU_DSH_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => saiku_core::userconfig::default_runtime_dir(&version),
    };
    let layout = RuntimeLayout::new(&dir, version);
    layout.check()?;
    Ok(layout)
}

/// 收掉内核，再用**进程表**独立确认它真的没了。
///
/// 两件事分开是有原因的：`Termination::exited` 说的是「我们自己 wait 到了子进程
/// 结束」，那对 node 直接子进程成立；而内核还会拉起工具子进程，`kill_tree` 那条路
/// 在 Windows 上只确认得了**根进程**（`process::kill_tree_by_pid` 只有 Unix 有「组空了」
/// 这个判据，Windows 那边看的是根 pid 有没有从进程表里消失）。所以这里自己走一遍：
/// 先请求收树，再轮询进程表。
async fn shutdown_and_confirm(kernel: &mut WebKernel, what: &str) {
    let pid = kernel.pid().expect("内核该有 pid");
    let started = Instant::now();
    let termination = kernel.shutdown().await;
    assert!(
        termination.exited,
        "{what}：收树没被确认 —— {}",
        termination.detail
    );

    // 收树请求本身幂等（进程不在了就是「已经没有东西了」），所以可以反复发；
    // 判据是**进程表**，不是那个请求的返回值。
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let _ = crate::process::kill_tree_by_pid(pid);
        if !pid_alive(pid) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "{what}：内核 pid {pid} 还在进程表里（shutdown 用了 {:?}）",
        started.elapsed()
    );
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    // `tasklist` 是 Windows 上唯一的通用查法；`OpenProcess` 需要 unsafe FFI，
    // 而本 crate 目前没有一处不安全的 Win32 调用（保留那个性质比省一次进程调用值钱）。
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(pid.to_string().as_str()),
        // 查不出来就**不要**说它活着 —— 会造出一个假失败。
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// 换票拿到的东西要**真的能用**，而且 `session/list` 在一个空 home 上回空列表。
///
/// 这一条压着三件事：带票 URL 的行形状、303 那一跳上的 `Set-Cookie`（跟随重定向
/// 就拿不到）、以及一元信封的 `args` 字段名（`session/list` 的形参就叫 `_request`，
/// 见 `packages/api/session-controller/src/index.ts:259-260`）。三件事任一错，
/// 表现都是「什么都没有」。
#[tokio::test]
#[ignore = "起真内核，需要本机已安装 DSH 运行时"]
async fn remote_live_kernel_starts_and_a_readonly_call_answers() {
    let Some((kernel, _home)) = start_kernel("remote_live_kernel_starts_and_a_readonly_call_answers").await
    else {
        return;
    };

    let rpc = RemoteRpc::for_kernel(&kernel).expect("建一元调用句柄");
    let value = rpc
        .call("session/list", json!({ "_request": {} }))
        .await
        .expect("session/list 该成功");

    // 空 home：新内核看不到任何会话。`items` 必须是**存在且为空**的数组 ——
    // 「字段不在」与「空」不是一回事，前者说明我们读错了地方。
    let items = value
        .get("items")
        .unwrap_or_else(|| panic!("session/list 的返回值里没有 items：{value}"));
    assert!(
        items.as_array().is_some_and(|items| items.is_empty()),
        "空 home 上的会话列表不是空数组：{value}"
    );

    let mut kernel = kernel;
    shutdown_and_confirm(&mut kernel, "只读一元调用之后").await;
}

/// mux 握手 + 一条**不需要已有会话**的只读流。
///
/// `session/control` 是 `$events` 之外唯一一条不要求会话身份的流
/// （`packages/api/session-controller/src/index.ts:557-560`：只吃 `signal`，
/// 没有业务参数）。它的第一个 item 是完整的控制基线，正好用来证明
/// 「握手过了、流开了、帧按 streamId 到了」。
///
/// **不碰 `session/follow`**：那条路会 promote 冷会话（P15），而这里没有会话可跟。
#[tokio::test]
#[ignore = "起真内核，需要本机已安装 DSH 运行时"]
async fn remote_live_mux_opens_a_readonly_stream_and_cancels_it() {
    let Some((kernel, _home)) = start_kernel("remote_live_mux_opens_a_readonly_stream_and_cancels_it").await
    else {
        return;
    };

    let mux = RemoteMux::connect(&kernel).await.expect("mux 握手");
    let mut stream = mux
        .open("session/control", json!({}))
        .await
        .expect("开一条 session/control 流");

    // 第一帧：控制基线。**给足时间** —— 内核刚起来，插件树还在收尾。
    let first = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .expect("60 秒内该收到基线帧")
        .expect("流不该在大声结束之前就没了");
    match first {
        RemoteFrame::Item(value) => {
            // 基线里必须有一个 `type`（`SessionControlFrame` 的判别字段）。
            // 只断言「是个带 type 的对象」而不是逐字段比对：那是下游
            // （会话操作）要按自己需要读的东西，这一层只保证「帧按流到了」。
            assert!(
                value.get("type").is_some(),
                "基线帧没有 type 判别字段：{value}"
            );
        }
        // 结构化失败也是**有效答案**（比如内核认为这个 peer 不允许控制流），
        // 只要它不是「协议错误」或「挂住」。所以这里只说清楚收到的是什么。
        RemoteFrame::Failure(failure) => {
            eprintln!("内核拒绝了 session/control：[{}] {}", failure.code, failure.message);
        }
        other => panic!("第一帧既不是 item 也不是 failure：{other:?}"),
    }

    // 关掉这一条流：内核那一侧的 Host 迭代器要跟着中止（`cancel`）。
    stream.close();
    assert_eq!(stream.next().await, None, "关掉的流不该再给帧");

    // 连接还在，而且能确认它没有静默挂断。
    assert!(mux.end_reason().is_none(), "连接不该已经断了：{:?}", mux.end_reason());
    mux.shutdown().await;

    let mut kernel = kernel;
    shutdown_and_confirm(&mut kernel, "mux 关掉之后").await;
}

/// 内核**没能起来**时要给出可读原因，而不是挂到超时。
///
/// 这一条不需要已安装的运行时 —— 它故意指一个不存在的入口。
/// 放在 live 组里是因为它守的是 host 那条**真起进程**的路径（离线那条只测了
/// 换票那一段函数）。
#[tokio::test]
#[ignore = "起真进程（但故意失败），需要 PATH 上有 node"]
async fn remote_live_a_missing_entry_fails_fast_with_a_readable_reason() {
    let Some(node) = find_node(Path::new(".")) else {
        eprintln!("跳过：PATH 上没有 `node`");
        return;
    };
    let home = tempfile::Builder::new()
        .prefix("saiku-remote-live-missing-")
        .tempdir()
        .expect("建临时 DSH_HOME");

    let missing = home.path().join("不存在的 bin.js");
    let options = WebKernelOptions::new(node, &missing, home.path());
    let started = Instant::now();
    let error = WebKernel::start(options).await.expect_err("入口不存在就该失败");
    let text = error.to_string();

    assert!(text.contains("找不到 DSH 入口"), "{text}");
    assert!(
        text.contains(&missing.display().to_string()),
        "错误里该带上找不到的那个路径：{text}"
    );
    // 「快速失败」是有内容的断言：`start` 的上限是 120 秒，而这条路径不该等它。
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "没入口还等了 {:?}",
        started.elapsed()
    );
}

/// 凭据不打码这件事**在真实内核上**也要成立。
///
/// 离线那条测的是 `redact` 这个函数；这一条测的是「真的起了一次内核之后，
/// 我们能拿到的对外文本里没有那一段令牌」。
#[tokio::test]
#[ignore = "起真内核，需要本机已安装 DSH 运行时"]
async fn remote_live_diagnostics_never_carry_the_launch_token() {
    let Some((mut kernel, _home)) = start_kernel("remote_live_diagnostics_never_carry_the_launch_token").await
    else {
        return;
    };

    // 内核的诊断尾巴（会进错误消息的那段文本）。
    let tail = kernel.stderr_tail();
    assert!(!tail.contains("?token="), "诊断尾巴里出现了带票 URL：{tail}");

    // `Debug` 是最容易被顺手塞进日志的一条路。
    let debugged = format!("{kernel:?}");
    assert!(debugged.contains("Secret(<redacted>)"), "{debugged}");
    assert!(!debugged.contains("?token="), "{debugged}");

    // mux / rpc 的 `Debug` 同理。
    let mux = RemoteMux::connect(&kernel).await.expect("mux 握手");
    assert!(!format!("{mux:?}").contains("v1."), "{mux:?}");
    let rpc = RemoteRpc::for_kernel(&kernel).expect("一元句柄");
    assert!(!format!("{rpc:?}").contains("v1."), "{rpc:?}");
    // cookie 是个 `Secret`：写死这一条是想让「哪天有人把它换成 `String`」
    // 在编译期就停下来。
    let _: &Secret = kernel.cookie();

    mux.shutdown().await;
    shutdown_and_confirm(&mut kernel, "凭据测试之后").await;
}

/// 审批通道的**真实**那一半：`$events` 这条 Gateway 内部流在 0.2.1-alpha.2 上开得起来，
/// 第一帧是 `ready` 且带 `clientId`。
///
/// 为什么这条必须对真内核跑：`$events` 只有一段（其余 Remote 方法都是 `ns/name`），
/// 我们自己的 `valid_method` 就因此把它挡下来一次；假服务端不会替我们回答
/// 「真实的 Gateway 认不认这个端点」。**不发 prompt、不需要密钥**。
#[tokio::test]
#[ignore = "起真实 web 内核（几十秒）；需要本机装了钉住运行时的机器"]
async fn remote_live_forwarded_events_stream_is_ready() {
    use crate::remote::session::SessionClient;

    let Some((mut kernel, _home)) = start_kernel("remote_live_forwarded_events_stream_is_ready").await
    else {
        return;
    };
    let client = SessionClient::connect(&kernel).await.expect("连上内核");
    let subscription = client.subscribe_events().await.expect("$events 应该开得起来");
    assert!(
        !subscription.client_id().is_empty(),
        "ready 帧必须带 clientId，否则之后没法回答任何 waterfall"
    );
    drop(subscription);
    shutdown_and_confirm(&mut kernel, "转发事件流之后").await;
}


/// 内核命令通道（`commands/list` + `commands/execute`）在真内核上的形状。
///
/// 不调模型、不耗配额：`/compact` 在空会话上只会被内核判成「没有可压缩的历史」，
/// 但**调用本身**要走通 —— 这是 TUI 的 `/kernel` 与未知命令转发的依据。
///
/// 用 `SAIKU_DSH_RUNTIME_DIR` 指向候选内核目录，就能拿同一套断言体检新版本。
#[tokio::test]
#[ignore = "起真内核，需要本机已安装 DSH 运行时"]
async fn remote_live_commands_answer_the_kernel_registry() {
    let Some((kernel, _home)) = start_kernel("remote_live_commands_answer_the_kernel_registry").await
    else {
        return;
    };
    let client = SessionClient::connect(&kernel).await.expect("连内核");
    let workspace = tempfile::tempdir().expect("建 workspace");
    let session = client.create(workspace.path()).await.expect("建会话");

    let items = client.list_commands(&session).await.expect("commands/list 该成功");
    assert!(!items.is_empty(), "内核没注册任何人类命令：{items:?}");
    let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
    assert!(
        names.contains(&"compact"),
        "命令表里应有 compact（升级内核时这一条最容易被改名）：{names:?}"
    );

    client
        .execute_command(&session, "/compact")
        .await
        .expect("commands/execute 该成功（空会话的结果由内核决定）");

    let mut kernel = kernel;
    shutdown_and_confirm(&mut kernel, "命令通道之后").await;
}


/// 附件与提问的 **Remote 端点是否存在**（能力探测，不做 UI）。
///
/// 这两件事一度被记成「要 0.2.2 才有」——实测 0.2.1 的 web profile 里
/// `user-questions` 与 `file-upload` 都已经加载。这条用例只回答「端点在不在」：
/// 用**必然失败的参数**去调，404 才算不存在；参数错、找不到调用，都说明端点是真的。
#[tokio::test]
#[ignore = "起真内核，需要本机已安装 DSH 运行时"]
async fn remote_live_attachment_and_question_endpoints_exist() {
    let Some((kernel, _home)) =
        start_kernel("remote_live_attachment_and_question_endpoints_exist").await
    else {
        return;
    };
    let rpc = RemoteRpc::for_kernel(&kernel).expect("建一元调用句柄");

    let answer = rpc
        .call(
            "userQuestions/answer",
            json!({
                "agentId": "no-such-agent",
                "callId": "no-such-call",
                "answer": { "answers": [] },
            }),
        )
        .await;
    if let Err(error) = &answer {
        assert!(
            !error.to_string().contains("404"),
            "userQuestions/answer 在这个内核上不存在：{error}"
        );
    }

    let upload = rpc
        .call(
            "fileUploads/upload",
            json!({ "agentId": "no-such-agent", "request": {} }),
        )
        .await;
    if let Err(error) = &upload {
        assert!(
            !error.to_string().contains("404"),
            "fileUploads/upload 在这个内核上不存在：{error}"
        );
    }

    // 打印真实结果（不看凭据，只看端点的答复）：人工跑时能一眼看出「在/不在」。
    match &answer {
        Ok(value) => eprintln!("userQuestions/answer → {value}"),
        Err(error) => eprintln!("userQuestions/answer → 错误：{error}"),
    }
    match &upload {
        Ok(value) => eprintln!("fileUploads/upload → {value}"),
        Err(error) => eprintln!("fileUploads/upload → 错误：{error}"),
    }
    // 把「能不能用」交给下一层（参数/权限/会话），这里只证存在。

    let mut kernel = kernel;
    shutdown_and_confirm(&mut kernel, "能力探测之后").await;
}

/// **端到端**：真内核 + 假模型端点，模型提问 → 事件瀑布 → 我们回答 → 模型继续。
///
/// 这是 F5 的验收：`ask_user` 在补丁里**没有被摘掉**（`blocking_tools: false`），
/// 所以这条用例证明的是「界面能答」，而不只是「解析器认得」。
/// 需要 `--features test-runtime`（假 provider 只在该 feature 下编译）。
#[cfg(feature = "test-runtime")]
#[tokio::test]
#[ignore = "起真内核 + 脚本化模型；需要本机已安装 DSH 运行时"]
async fn remote_live_a_question_is_answered_through_the_waterfall() {
    use crate::fake_provider::{Provider, Script};
    use crate::remote::launch::policy_for;
    use crate::runtime::{render_patch_with_routes, PatchInput};
    use saiku_core::authz::Permission;
    use saiku_core::config::{RouteModel, RouteSpec};
    use saiku_core::conversation::{ModelChoice, QuestionAnswer};
    use saiku_core::mode::{AgentId, Mode};

    let Ok(layout) = runtime_layout() else {
        eprintln!("跳过：本机没有要的运行时");
        return;
    };
    let Some(node) = find_node(&layout.dir) else {
        eprintln!("跳过：找不到 node");
        return;
    };

    let dir = tempfile::tempdir().expect("临时目录");
    let script = Script::parse(
        r#"{"responses":[
            {"blocks":[{"tool":"ask_user_question","arguments":{"questions":[
                {"id":"pick","question":"选哪个？","options":[{"label":"A"},{"label":"B"}]}
            ]}}]},
            {"blocks":[{"text":"收到，按 B 做。"}]}
        ]}"#,
    )
    .expect("脚本该能解析");
    let provider = Provider::start(script, dir.path().join("provider.jsonl")).expect("起假端点");

    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("建 workspace");
    let spec = RouteSpec {
        provider_id: "fake-endpoint".into(),
        adapter: "pi-ai".into(),
        api: Some("anthropic-messages".into()),
        base_url: Some(provider.url()),
        headers: Default::default(),
        models: vec![RouteModel { id: "fake-model".into(), name: Some("Fake".into()), context_window: Some(100_000) }],
        display_name: None,
        key_env: "DSH_TEST_KEY".into(),
        key_source: "env:DSH_TEST_KEY".into(),
        auth_id: None,
    };
    let policy = policy_for(Permission::Full, AgentId::Build, Mode::Build, &workspace);
    let guard = policy.execution_guard(Permission::Full).expect("算 guard");
    let patch = render_patch_with_routes(
        &PatchInput {
            route: &spec,
            persona_suffix: "",
            session_log_upload: false,
            guard: &guard,
            session_search: false,
            // **故意不摘** ask_user：这条用例要证明界面真能回答。
            blocking_tools: false,
        },
        &[],
    );
    let patch_path = dir.path().join("patch.json");
    std::fs::write(&patch_path, patch).expect("写 patch");

    let home = tempfile::tempdir().expect("建 DSH_HOME");
    let mut options = WebKernelOptions::new(node, layout.entry(), home.path());
    options.patch = Some(patch_path);
    options.env.insert("DSH_TEST_KEY".into(), "test-key".into());
    options.cwd = Some(workspace.clone());
    options.startup_timeout = LIVE_STARTUP_TIMEOUT;
    let mut kernel = WebKernel::start(options).await.expect("起 web 内核");

    let client = SessionClient::connect(&kernel).await.expect("连内核");
    let session = client.create(&workspace).await.expect("建会话");
    client
        .select_model(
            &session,
            &ModelChoice { provider: "fake-endpoint".into(), model: "fake-model".into(), effort: None },
        )
        .await
        .expect("选模型");
    let mut subscription = client.subscribe_events().await.expect("订阅事件");
    client.prompt(&session, "请向我提问").await.expect("发 prompt");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut answered = false;
    while tokio::time::Instant::now() < deadline && !answered {
        match tokio::time::timeout(Duration::from_secs(10), subscription.next()).await {
            Ok(Forwarded::Question(request)) => {
                assert_eq!(request.questions[0].id, "pick");
                let rpc = RemoteRpc::for_kernel(&kernel).expect("建调用句柄");
                crate::remote::events::answer_question(
                    &rpc,
                    subscription.client_id(),
                    &request.request_id,
                    &[QuestionAnswer { id: "pick".into(), selected: vec!["B".into()], custom: None }],
                )
                .await
                .expect("回答该送达");
                answered = true;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(answered, "模型问了，但没等到提问瀑布");

    // 等模型那侧真的收到回答（工具结果里应带 B）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut seen = false;
    while tokio::time::Instant::now() < deadline && !seen {
        seen = provider
            .requests()
            .iter()
            .any(|request| crate::fake_provider::transcript(&request["body"]).contains('B'));
        if !seen {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    assert!(seen, "模型没收到回答：{:?}", provider.requests().len());

    shutdown_and_confirm(&mut kernel, "提问往返之后").await;
}
