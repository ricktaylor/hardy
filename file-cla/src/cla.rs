use super::*;
use hardy_bpa::async_trait;

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Invalid path '{0}'")]
    BadPath(String),
}

fn check_path(cwd: &Path, path: &PathBuf) -> hardy_bpa::cla::Result<String> {
    let path = cwd.join(path);

    // Check everything is UTF-8
    if path.to_str().is_none() {
        error!("Ignoring invalid path '{}'", path.display());
        return Err(hardy_bpa::cla::Error::Internal(
            Error::BadPath(format!("{}", path.display())).into(),
        ));
    }

    // Ensure we have created the path
    std::fs::create_dir_all(&path).map_err(|e| {
        error!("Failed to create directory 'path': {e}");
        hardy_bpa::cla::Error::Internal(e.into())
    })?;

    let path = path.canonicalize().map_err(|e| {
        error!("Failed to canonicalise path '{}': {e}'", path.display());
        hardy_bpa::cla::Error::Internal(e.into())
    })?;

    Ok(path.to_string_lossy().into_owned())
}

#[async_trait]
impl hardy_bpa::cla::Cla for Cla {
    async fn on_register(
        &self,
        sink: Box<dyn hardy_bpa::cla::Sink>,
        _node_ids: &[Eid],
    ) -> hardy_bpa::cla::Result<()> {
        let cwd = std::env::current_dir().map_err(|e| {
            error!("Failed to get current working directory: {e}");
            hardy_bpa::cla::Error::Internal(e.into())
        })?;

        let mut inboxes = HashSet::new();
        for (eid, path) in &self.config.peers {
            let path = check_path(&cwd, path)?;

            // Register the peer with the BPA
            sink.add_peer(
                eid.clone(),
                hardy_bpa::cla::ClaAddress::Private(hardy_bpa::Bytes::copy_from_slice(
                    path.as_bytes(),
                )),
            )
            .await?;

            inboxes.insert(path);
        }

        self.inner.set(ClaInner { inboxes }).map_err(|_| {
            error!("CLA on_register called twice!");
            hardy_bpa::cla::Error::AlreadyConnected
        })?;

        self.start_watcher(sink.into(), check_path(&cwd, &self.config.outbox)?)
            .await;

        Ok(())
    }

    async fn on_unregister(&self) {
        self.cancel_token.cancel();
        self.task_tracker.close();
        self.task_tracker.wait().await;
    }

    async fn forward(
        &self,
        _queue: Option<u32>,
        cla_addr: &hardy_bpa::cla::ClaAddress,
        bundle: hardy_bpa::Bytes,
    ) -> hardy_bpa::cla::Result<hardy_bpa::cla::ForwardBundleResult> {
        let inner = self.inner.get().ok_or_else(|| {
            error!("forward called before on_register!");
            hardy_bpa::cla::Error::Disconnected
        })?;

        if let hardy_bpa::cla::ClaAddress::Private(remote_addr) = cla_addr
            && let Ok(path) = str::from_utf8(remote_addr.as_ref())
            && inner.inboxes.contains(path)
        {
            // Put bundle into outbox
            let path = match hardy_bpv7::bundle::Id::parse(&bundle) {
                Ok(id) => {
                    let mut filename = format!("{}_{}", id.source, id.timestamp)
                        .replace(['\\', '/', ':', ' '], "_");
                    if let Some(fragment_info) = id.fragment_info {
                        filename.push_str(format!("_fragment_{}", fragment_info.offset).as_str());
                    }
                    PathBuf::from(path).join(filename)
                }
                Err(e) => {
                    warn!("Ignoring invalid bundle: {e}");
                    return Err(e.into());
                }
            };

            return tokio::fs::write(&path, bundle)
                .await
                .map(|_| hardy_bpa::cla::ForwardBundleResult::Sent)
                .map_err(|e| {
                    error!("Failed to write to '{}': {e}", path.display());
                    hardy_bpa::cla::Error::Internal(e.into())
                });
        }

        Ok(hardy_bpa::cla::ForwardBundleResult::NoNeighbour)
    }
}
