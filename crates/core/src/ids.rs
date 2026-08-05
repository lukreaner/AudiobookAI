use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! domain_id {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Deserialize,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

domain_id!(ArtifactId);
domain_id!(AttemptId);
domain_id!(BookId);
domain_id!(BudgetId);
domain_id!(CapabilitySnapshotId);
domain_id!(ChapterId);
domain_id!(CharacterId);
domain_id!(DetectionRunId);
domain_id!(DictionaryId);
domain_id!(DictionaryRuleId);
domain_id!(ExportProfileId);
domain_id!(ExportPackageId);
domain_id!(JobId);
domain_id!(JobUnitId);
domain_id!(ParagraphId);
domain_id!(ProjectId);
domain_id!(ProviderProfileId);
domain_id!(RateCardId);
domain_id!(ReservationId);
domain_id!(SecretId);
domain_id!(SegmentId);
domain_id!(SegmentTakeId);
domain_id!(SessionId);
domain_id!(SpeakerOverrideId);
domain_id!(UsageEventId);
domain_id!(ProofExportSnapshotId);
domain_id!(QualityReportId);
domain_id!(VoiceAssignmentId);
domain_id!(VoiceProfileId);
