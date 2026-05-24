#[tokio::test]
async fn communicate_comment_plan_clears_proposal_and_notifies_proposer() {
    let _env_lock = crate::storage::lock_test_env();
    let runtime_dir = tempfile::TempDir::new().expect("runtime tempdir");
    let repo_dir = std::env::current_dir().expect("repo cwd");
    let socket_path = runtime_dir.path().join("jcode.sock");
    let _runtime = EnvGuard::set("JCODE_RUNTIME_DIR", runtime_dir.path());
    let _socket = EnvGuard::set("JCODE_SOCKET", &socket_path);
    let _debug = EnvGuard::set("JCODE_DEBUG_CONTROL", "1");

    let provider: Arc<dyn Provider> = Arc::new(DelayedTestProvider {
        delay: Duration::from_millis(50),
    });
    let server = Arc::new(Server::new(provider));
    let mut server_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };

    wait_for_server_socket(&socket_path, &mut server_task)
        .await
        .expect("server socket should be ready");

    let mut coordinator = RawClient::connect(&socket_path)
        .await
        .expect("coordinator should connect");
    let mut proposer = RawClient::connect(&socket_path)
        .await
        .expect("proposer should connect");
    coordinator
        .subscribe(&repo_dir)
        .await
        .expect("coordinator subscribe");
    proposer
        .subscribe(&repo_dir)
        .await
        .expect("proposer subscribe");

    let coordinator_session = coordinator
        .session_id()
        .await
        .expect("coordinator session id");
    let proposer_session = proposer.session_id().await.expect("proposer session id");
    let tool = CommunicateTool::new();
    let coordinator_ctx = test_ctx(&coordinator_session, &repo_dir);
    let proposer_ctx = test_ctx(&proposer_session, &repo_dir);

    tool.execute(
        json!({
            "action": "assign_role",
            "target_session": coordinator_session,
            "role": "coordinator"
        }),
        coordinator_ctx.clone(),
    )
    .await
    .expect("coordinator self-promotion should succeed");

    tool.execute(
        json!({
            "action": "propose_plan",
            "plan_items": [{
                "id": "task-a",
                "content": "Validate inputs",
                "status": "queued",
                "priority": "high"
            }]
        }),
        proposer_ctx,
    )
    .await
    .expect("plan proposal should succeed");

    coordinator
        .read_until(Duration::from_secs(5), |event| {
            matches!(
                event,
                ServerEvent::SwarmPlanProposal {
                    proposer_session: session,
                    ..
                } if session == &proposer_session
            )
        })
        .await
        .expect("coordinator should receive proposal event");

    let output = tool
        .execute(
            json!({
                "action": "comment_plan",
                "proposer_session": proposer_session.clone(),
                "plan_comments": [{
                    "range_start": 0,
                    "range_end": 0,
                    "text": "Split validation into a separate task.",
                    "quote": "Validate inputs"
                }]
            }),
            coordinator_ctx.clone(),
        )
        .await
        .expect("comment_plan should succeed");
    assert!(output.output.contains("Sent plan feedback"));

    let feedback = proposer
        .read_until(Duration::from_secs(5), |event| {
            matches!(
                event,
                ServerEvent::Notification {
                    notification_type: NotificationType::Message {
                        scope: Some(scope),
                        ..
                    },
                    message,
                    ..
                } if scope == "plan_feedback" && message.contains("requested changes")
            )
        })
        .await
        .expect("proposer should receive feedback");
    let ServerEvent::Notification { message, .. } = feedback else {
        panic!("expected notification");
    };
    assert!(message.contains("Split validation"));

    let stale_approval = tool
        .execute(
            json!({
                "action": "approve_plan",
                "proposer_session": proposer_session
            }),
            coordinator_ctx,
        )
        .await
        .expect_err("commenting should clear stale approval");
    assert!(stale_approval
        .to_string()
        .contains("No pending plan proposal"));

    server_task.abort();
}

#[tokio::test]
async fn communicate_structured_question_routes_answer_back_to_asker() {
    let _env_lock = crate::storage::lock_test_env();
    let runtime_dir = tempfile::TempDir::new().expect("runtime tempdir");
    let repo_dir = std::env::current_dir().expect("repo cwd");
    let socket_path = runtime_dir.path().join("jcode.sock");
    let _runtime = EnvGuard::set("JCODE_RUNTIME_DIR", runtime_dir.path());
    let _socket = EnvGuard::set("JCODE_SOCKET", &socket_path);
    let _debug = EnvGuard::set("JCODE_DEBUG_CONTROL", "1");

    let provider: Arc<dyn Provider> = Arc::new(DelayedTestProvider {
        delay: Duration::from_millis(50),
    });
    let server = Arc::new(Server::new(provider));
    let mut server_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };

    wait_for_server_socket(&socket_path, &mut server_task)
        .await
        .expect("server socket should be ready");

    let mut asker = RawClient::connect(&socket_path)
        .await
        .expect("asker should connect");
    let mut target = RawClient::connect(&socket_path)
        .await
        .expect("target should connect");
    asker.subscribe(&repo_dir).await.expect("asker subscribe");
    target.subscribe(&repo_dir).await.expect("target subscribe");

    let asker_session = asker.session_id().await.expect("asker session id");
    let target_session = target.session_id().await.expect("target session id");
    let tool = CommunicateTool::new();
    let asker_ctx = test_ctx(&asker_session, &repo_dir);

    let output = tool
        .execute(
            json!({
                "action": "ask",
                "to_session": target_session.clone(),
                "question_id": "q-scope",
                "message": "Pick implementation scope",
                "options": [{
                    "id": "small",
                    "label": "Small",
                    "description": "Minimal change"
                }],
                "allow_freeform": true
            }),
            asker_ctx,
        )
        .await
        .expect("ask should succeed");
    assert!(output.output.contains("q-scope"));

    let question = target
        .read_until(Duration::from_secs(5), |event| {
            matches!(
                event,
                ServerEvent::WorkflowQuestion {
                    question_id,
                    from_session,
                    ..
                } if question_id == "q-scope" && from_session == &asker_session
            )
        })
        .await
        .expect("target should receive workflow question");
    let ServerEvent::WorkflowQuestion {
        options,
        allow_freeform,
        ..
    } = question
    else {
        panic!("expected workflow question");
    };
    assert_eq!(options[0].id, "small");
    assert!(allow_freeform);

    let answer_id = target.next_id;
    target.next_id += 1;
    target
        .send_request(Request::WorkflowAnswerQuestion {
            id: answer_id,
            from_session: target_session.clone(),
            to_session: asker_session.clone(),
            answer: WorkflowQuestionAnswer {
                question_id: "q-scope".to_string(),
                option_id: Some("small".to_string()),
                answer_text: "Small".to_string(),
            },
        })
        .await
        .expect("answer should send");
    target
        .wait_for_done(answer_id)
        .await
        .expect("answer request should complete");

    let answer = asker
        .read_until(Duration::from_secs(5), |event| {
            matches!(
                event,
                ServerEvent::WorkflowQuestionAnswered {
                    question_id,
                    from_session,
                    ..
                } if question_id == "q-scope" && from_session == &target_session
            )
        })
        .await
        .expect("asker should receive workflow answer");
    let ServerEvent::WorkflowQuestionAnswered { answer, .. } = answer else {
        panic!("expected workflow answer");
    };
    assert_eq!(answer.option_id.as_deref(), Some("small"));
    assert_eq!(answer.answer_text, "Small");

    server_task.abort();
}
