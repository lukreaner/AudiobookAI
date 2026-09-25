use super::{
    AppState, Arc, Duration, Event, ExportArtifactView, HeaderMap, Infallible, IntoResponse,
    JobView, Json, Message, Page, Path, Response, ServiceError, SinkExt, Sse, StartJobInput, State,
    StatusCode, Stream, Uuid, WebSocketUpgrade, active_job_conflict, blocking_character_job,
    dry_run_project, event_stream, extend_dry_run_environment,
};

pub(super) async fn list_jobs(State(state): State<Arc<AppState>>) -> Json<Page<JobView>> {
    let catalog = state.catalog.read().await;
    let mut jobs = catalog.jobs.values().cloned().collect::<Vec<_>>();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
    Json(Page::all(jobs))
}

pub(super) async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobView>, ServiceError> {
    state
        .catalog
        .read()
        .await
        .jobs
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(ServiceError::NotFound)
}

pub(super) async fn start_job(
    State(state): State<Arc<AppState>>,
    Json(input): Json<StartJobInput>,
) -> Result<(StatusCode, Json<JobView>), ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(input.project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, input.project_id).await {
        return Err(active_job_conflict(&job));
    }
    let (project, characters, providers) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&input.project_id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog
                .characters
                .get(&input.project_id)
                .cloned()
                .unwrap_or_default(),
            catalog.providers.clone(),
        )
    };
    let dry_run = extend_dry_run_environment(
        &state,
        &project,
        &characters,
        &providers,
        &input.export,
        dry_run_project(&project, &characters, &providers),
    )
    .await?;
    if !dry_run.ready {
        return Err(ServiceError::Conflict(
            "dry-run checks for the current export settings must pass before conversion".to_owned(),
        ));
    }
    let job = crate::conversion::start_conversion(Arc::clone(&state), input).await?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

pub(super) async fn job_action(
    State(state): State<Arc<AppState>>,
    Path((id, action)): Path<(Uuid, String)>,
) -> Result<Json<JobView>, ServiceError> {
    crate::conversion::job_action(state, id, &action)
        .await
        .map(Json)
}

pub(super) async fn job_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ServiceError> {
    if !state.catalog.read().await.jobs.contains_key(&id) {
        return Err(ServiceError::NotFound);
    }
    Ok(event_stream(state, Some(id)))
}

pub(super) async fn job_playback(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ServiceError> {
    if !state.catalog.read().await.jobs.contains_key(&id) {
        return Err(ServiceError::NotFound);
    }
    Ok(ws
        .on_upgrade(move |mut socket| async move {
            let mut receiver = crate::conversion::subscribe_playback(id);
            loop {
                tokio::select! {
                    playback_event = receiver.recv() => match playback_event {
                        Ok(crate::conversion::PlaybackPacket::Audio(chunk)) => {
                            if socket.send(Message::Binary(chunk)).await.is_err() {
                                break;
                            }
                        }
                        Ok(crate::conversion::PlaybackPacket::Reset) => {
                            if socket.send(Message::Text(r#"{"type":"reset"}"#.into())).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    () = tokio::time::sleep(Duration::from_millis(500)) => {
                        if crate::conversion::job_is_terminal(&state, id).await {
                            break;
                        }
                    }
                }
            }
            let _ = socket.close().await;
        })
        .into_response())
}

pub(super) async fn artifact_download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, ServiceError> {
    crate::conversion::artifact_response(&state, id, &headers).await
}

pub(super) async fn list_exports(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Page<ExportArtifactView>>, ServiceError> {
    crate::conversion::list_exports(&state)
        .await
        .map(Page::all)
        .map(Json)
}
