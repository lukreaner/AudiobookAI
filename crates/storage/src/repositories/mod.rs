mod budgets;
mod idempotency;
mod jobs;
mod projects;
mod providers;
mod usage;
mod util;

pub use budgets::BudgetRepository;
pub use idempotency::{IdempotencyClaim, IdempotencyRepository, IdempotentResponse};
pub use jobs::JobRepository;
pub use projects::ProjectRepository;
pub use providers::ProviderRepository;
pub use usage::{UsageFilter, UsageRepository};

use sqlx::SqlitePool;

#[derive(Clone, Debug)]
pub struct Repositories {
    pub projects: ProjectRepository,
    pub providers: ProviderRepository,
    pub jobs: JobRepository,
    pub usage: UsageRepository,
    pub budgets: BudgetRepository,
    pub idempotency: IdempotencyRepository,
}

impl Repositories {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            projects: ProjectRepository::new(pool.clone()),
            providers: ProviderRepository::new(pool.clone()),
            jobs: JobRepository::new(pool.clone()),
            usage: UsageRepository::new(pool.clone()),
            budgets: BudgetRepository::new(pool.clone()),
            idempotency: IdempotencyRepository::new(pool),
        }
    }
}
