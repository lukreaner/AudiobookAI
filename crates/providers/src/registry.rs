use std::{collections::BTreeMap, fmt, sync::Arc};

use tokio::sync::RwLock;

use crate::{
    CharacterProvider, ProviderControl, ProviderError, ProviderId, Result, TtsProvider,
    VoiceCloneProvider,
};

/// Thread-safe registry used by the embedded service and provider runtime.
#[derive(Default)]
pub struct ProviderRegistry {
    tts: RwLock<BTreeMap<ProviderId, Arc<dyn TtsProvider>>>,
    character: RwLock<BTreeMap<ProviderId, Arc<dyn CharacterProvider>>>,
    controls: RwLock<BTreeMap<ProviderId, Arc<dyn ProviderControl>>>,
    clones: RwLock<BTreeMap<ProviderId, Arc<dyn VoiceCloneProvider>>>,
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRegistry").finish_non_exhaustive()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register_tts(&self, provider: Arc<dyn TtsProvider>) -> Result<()> {
        insert_unique(&self.tts, provider.descriptor().id.clone(), provider).await
    }

    pub async fn register_character(&self, provider: Arc<dyn CharacterProvider>) -> Result<()> {
        insert_unique(&self.character, provider.descriptor().id.clone(), provider).await
    }

    pub async fn register_control(&self, provider: Arc<dyn ProviderControl>) -> Result<()> {
        insert_unique(&self.controls, provider.descriptor().id.clone(), provider).await
    }

    pub async fn register_voice_cloner(&self, provider: Arc<dyn VoiceCloneProvider>) -> Result<()> {
        insert_unique(&self.clones, provider.descriptor().id.clone(), provider).await
    }

    pub async fn tts(&self, id: &ProviderId) -> Option<Arc<dyn TtsProvider>> {
        self.tts.read().await.get(id).cloned()
    }

    pub async fn character(&self, id: &ProviderId) -> Option<Arc<dyn CharacterProvider>> {
        self.character.read().await.get(id).cloned()
    }

    pub async fn control(&self, id: &ProviderId) -> Option<Arc<dyn ProviderControl>> {
        self.controls.read().await.get(id).cloned()
    }

    pub async fn voice_cloner(&self, id: &ProviderId) -> Option<Arc<dyn VoiceCloneProvider>> {
        self.clones.read().await.get(id).cloned()
    }

    pub async fn tts_ids(&self) -> Vec<ProviderId> {
        self.tts.read().await.keys().cloned().collect()
    }

    pub async fn character_ids(&self) -> Vec<ProviderId> {
        self.character.read().await.keys().cloned().collect()
    }
}

async fn insert_unique<T: ?Sized>(
    map: &RwLock<BTreeMap<ProviderId, Arc<T>>>,
    id: ProviderId,
    provider: Arc<T>,
) -> Result<()> {
    let mut map = map.write().await;
    if map.contains_key(&id) {
        return Err(ProviderError::Configuration(format!(
            "provider {id} is already registered"
        )));
    }
    map.insert(id, provider);
    Ok(())
}
