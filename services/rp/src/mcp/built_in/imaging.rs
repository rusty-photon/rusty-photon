use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::internals::{
    star_to_json, ResolvedClipParams, ResolvedDetectParams, ResolvedMeasureStarsParams,
    ResolvedParams,
};
use super::super::{tool_error, tool_success};
use crate::imaging;

/// Canonical error text for missing image-source inputs. Centralised so
/// every tool surfaces the same message and a future tweak only needs to
/// land in one place.
const MISSING_SOURCE_ERROR: &str =
    "missing required argument: provide either document_id or image_path";

/// Resolved image source: either an exposure-document id from the cache
/// or a filesystem path. Constructed exclusively via
/// [`require_image_source`] so a `(None, None)` input becomes an `Err`
/// before the tool's `match` runs — downstream code only has to handle
/// the two reachable variants, no defensive arm required.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ImageSource<'a> {
    Document(&'a str),
    Path(&'a str),
}

/// Resolve the `(document_id, image_path)` parameter pair into a typed
/// [`ImageSource`]. `document_id` wins when both are supplied (matches
/// the documented param semantics). `(None, None)` is the only input
/// that errors.
pub(super) const fn require_image_source<'a>(
    document_id: Option<&'a str>,
    image_path: Option<&'a str>,
) -> Result<ImageSource<'a>, &'static str> {
    match (document_id, image_path) {
        (Some(d), _) => Ok(ImageSource::Document(d)),
        (None, Some(p)) => Ok(ImageSource::Path(p)),
        (None, None) => Err(MISSING_SOURCE_ERROR),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComputeImageStatsParams {
    /// Exposure-document id to resolve through the unified image+document
    /// cache. Wins over `image_path` when both are supplied. When set,
    /// the computed stats are written into the exposure document as an
    /// `image_stats` section.
    #[serde(default)]
    pub document_id: Option<String>,
    /// FITS file on disk (read via `rp-fits`). Used when `document_id`
    /// is absent or doesn't resolve through the cache.
    #[serde(default)]
    pub image_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeasureBasicParams {
    /// Exposure-document id to resolve through the unified image+document
    /// cache. Wins over `image_path` when both are supplied.
    #[serde(default)]
    pub document_id: Option<String>,
    /// FITS file on disk (read via `rp-fits`). Used when `document_id`
    /// is absent or doesn't resolve through the cache.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Detection threshold above background, expressed as multiples of the
    /// sigma-clipped background stddev. Default `5.0`.
    #[serde(default = "default_threshold_sigma")]
    pub threshold_sigma: f64,
    /// Minimum component pixel area to admit as a star. No default — value
    /// depends on per-rig pixel scale and seeing, neither of which the
    /// tool can infer from the image alone. The body validates presence
    /// (rather than relying on serde-required) so the missing-parameter
    /// error message is consistent with `image_path`/`document_id` cases.
    #[serde(default)]
    pub min_area: Option<usize>,
    /// Maximum component pixel area. Same rationale as `min_area`.
    #[serde(default)]
    pub max_area: Option<usize>,
}

const fn default_threshold_sigma() -> f64 {
    5.0
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EstimateBackgroundParams {
    /// Exposure-document id to resolve through the unified image+document
    /// cache. Wins over `image_path` when both are supplied.
    #[serde(default)]
    pub document_id: Option<String>,
    /// FITS file on disk (read via `rp-fits`). Used when `document_id`
    /// is absent or doesn't resolve through the cache.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Sigma-clip threshold in stddev units. Default `3.0`.
    #[serde(default = "default_clip_k")]
    pub k: f64,
    /// Maximum clip iterations. Default `5`.
    #[serde(default = "default_clip_max_iters")]
    pub max_iters: u32,
}

const fn default_clip_k() -> f64 {
    3.0
}
const fn default_clip_max_iters() -> u32 {
    5
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeasureStarsParams {
    /// Exposure-document id to resolve through the unified image+document
    /// cache. Wins over `image_path` when both are supplied.
    #[serde(default)]
    pub document_id: Option<String>,
    /// FITS file on disk (read via `rp-fits`). Used when `document_id`
    /// is absent or doesn't resolve through the cache.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Detection threshold above background, expressed as multiples of the
    /// sigma-clipped background stddev. Default `5.0`.
    #[serde(default = "default_threshold_sigma")]
    pub threshold_sigma: f64,
    /// Minimum component pixel area. No default — same rationale as
    /// `measure_basic`.
    #[serde(default)]
    pub min_area: Option<usize>,
    /// Maximum component pixel area. Same rationale.
    #[serde(default)]
    pub max_area: Option<usize>,
    /// Half-side of the postage stamp used for the 2D Gaussian fit.
    /// Default `8` (gives a 17×17 stamp).
    #[serde(default = "default_stamp_half_size")]
    pub stamp_half_size: usize,
}

const fn default_stamp_half_size() -> usize {
    imaging::DEFAULT_STAMP_HALF_SIZE
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComputeSnrParams {
    /// Exposure-document id to resolve through the unified image+document
    /// cache. Wins over `image_path` when both are supplied.
    #[serde(default)]
    pub document_id: Option<String>,
    /// FITS file on disk (read via `rp-fits`). Used when `document_id`
    /// is absent or doesn't resolve through the cache.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Detection threshold above background, expressed as multiples of the
    /// sigma-clipped background stddev. Default `5.0`.
    #[serde(default = "default_threshold_sigma")]
    pub threshold_sigma: f64,
    /// Minimum component pixel area. Same rationale as `measure_basic`.
    #[serde(default)]
    pub min_area: Option<usize>,
    /// Maximum component pixel area. Same rationale.
    #[serde(default)]
    pub max_area: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DetectStarsParams {
    /// Exposure-document id to resolve through the unified image+document
    /// cache. Wins over `image_path` when both are supplied.
    #[serde(default)]
    pub document_id: Option<String>,
    /// FITS file on disk (read via `rp-fits`). Used when `document_id`
    /// is absent or doesn't resolve through the cache.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Detection threshold above background, expressed as multiples of the
    /// sigma-clipped background stddev. Default `5.0`.
    #[serde(default = "default_threshold_sigma")]
    pub threshold_sigma: f64,
    /// Minimum component pixel area to admit as a star. Required.
    #[serde(default)]
    pub min_area: Option<usize>,
    /// Maximum component pixel area to admit. Required.
    #[serde(default)]
    pub max_area: Option<usize>,
}

#[tool_router(router = tool_router_imaging, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Read FITS file and compute pixel statistics (median, mean, min, max ADU)"
    )]
    pub(crate) async fn compute_image_stats(
        &self,
        Parameters(params): Parameters<ComputeImageStatsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source =
            match require_image_source(params.document_id.as_deref(), params.image_path.as_deref())
            {
                Ok(s) => s,
                Err(msg) => return Ok(tool_error!("{}", msg)),
            };

        let stats = match source {
            ImageSource::Document(doc_id) => match self.stats_via_document(doc_id).await {
                Ok(s) => s,
                Err(e) => return Ok(tool_error!("failed to compute stats: {}", e)),
            },
            ImageSource::Path(path) => match self.stats_via_path(path).await {
                Ok(s) => s,
                Err(e) => return Ok(tool_error!("failed to compute stats: {}", e)),
            },
        };

        debug!(
            document_id = params.document_id.as_deref().unwrap_or(""),
            image_path = params.image_path.as_deref().unwrap_or(""),
            median = stats.median_adu,
            mean = %stats.mean_adu,
            "computed image stats"
        );

        let payload = serde_json::json!({
            "median_adu": stats.median_adu,
            "mean_adu": stats.mean_adu,
            "min_adu": stats.min_adu,
            "max_adu": stats.max_adu,
            "pixel_count": stats.pixel_count,
        });

        if let Some(doc_id) = params.document_id.as_deref() {
            if let Err(e) = self
                .image_cache
                .put_section(doc_id, "image_stats", payload.clone())
                .await
            {
                debug!(error = %e, document_id = %doc_id, "failed to persist image_stats section");
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }

    #[tool(
        description = "Detect stars and compute HFR / sigma-clipped background statistics on a captured image"
    )]
    pub(crate) async fn measure_basic(
        &self,
        Parameters(params): Parameters<MeasureBasicParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source =
            match require_image_source(params.document_id.as_deref(), params.image_path.as_deref())
            {
                Ok(s) => s,
                Err(msg) => return Ok(tool_error!("{}", msg)),
            };
        let Some(min_area) = params.min_area else {
            return Ok(tool_error!("missing required parameter: min_area"));
        };
        let Some(max_area) = params.max_area else {
            return Ok(tool_error!("missing required parameter: max_area"));
        };
        let resolved = ResolvedParams {
            threshold_sigma: params.threshold_sigma,
            min_area,
            max_area,
        };

        let result = match source {
            ImageSource::Document(doc_id) => {
                match self.measure_via_document(doc_id, &resolved).await {
                    Ok(r) => r,
                    Err(e) => return Ok(tool_error!("{}", e)),
                }
            }
            ImageSource::Path(path) => match self.measure_via_path(path, &resolved).await {
                Ok(r) => r,
                Err(e) => return Ok(tool_error!("{}", e)),
            },
        };

        if let Some(doc_id) = params.document_id.as_deref() {
            let value = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
            if let Err(e) = self
                .image_cache
                .put_section(doc_id, "image_analysis", value)
                .await
            {
                debug!(error = %e, document_id = %doc_id, "failed to persist image_analysis section");
            }
        }

        Ok(tool_success!({
            "hfr": result.hfr,
            "star_count": result.star_count,
            "saturated_star_count": result.saturated_star_count,
            "background_mean": result.background_mean,
            "background_stddev": result.background_stddev,
            "pixel_count": result.pixel_count,
        }))
    }

    #[tool(description = "Sigma-clipped background mean / stddev / median for a captured image")]
    pub(crate) async fn estimate_background(
        &self,
        Parameters(params): Parameters<EstimateBackgroundParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source =
            match require_image_source(params.document_id.as_deref(), params.image_path.as_deref())
            {
                Ok(s) => s,
                Err(msg) => return Ok(tool_error!("{}", msg)),
            };
        if !params.k.is_finite() || params.k <= 0.0 {
            return Ok(tool_error!("invalid parameter: k must be > 0"));
        }
        if params.max_iters == 0 {
            return Ok(tool_error!("invalid parameter: max_iters must be >= 1"));
        }
        let resolved = ResolvedClipParams {
            k: params.k,
            max_iters: usize::try_from(params.max_iters).unwrap_or(usize::MAX),
        };

        let outcome = match source {
            ImageSource::Document(doc_id) => {
                match self.estimate_via_document(doc_id, &resolved).await {
                    Ok(s) => s,
                    Err(e) => return Ok(tool_error!("{}", e)),
                }
            }
            ImageSource::Path(path) => match self.estimate_via_path(path, &resolved).await {
                Ok(s) => s,
                Err(e) => return Ok(tool_error!("{}", e)),
            },
        };

        let payload = serde_json::json!({
            "mean": outcome.stats.mean,
            "stddev": outcome.stats.stddev,
            "median": outcome.stats.median,
            "pixel_count": outcome.total_pixels,
        });

        if let Some(doc_id) = params.document_id.as_deref() {
            if let Err(e) = self
                .image_cache
                .put_section(doc_id, "background", payload.clone())
                .await
            {
                debug!(error = %e, document_id = %doc_id, "failed to persist background section");
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }

    #[tool(
        description = "Detect stars on a captured image and return per-star coordinates, flux, peak, and saturation flags"
    )]
    pub(crate) async fn detect_stars(
        &self,
        Parameters(params): Parameters<DetectStarsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source =
            match require_image_source(params.document_id.as_deref(), params.image_path.as_deref())
            {
                Ok(s) => s,
                Err(msg) => return Ok(tool_error!("{}", msg)),
            };
        let Some(min_area) = params.min_area else {
            return Ok(tool_error!("missing required parameter: min_area"));
        };
        let Some(max_area) = params.max_area else {
            return Ok(tool_error!("missing required parameter: max_area"));
        };
        let resolved = ResolvedDetectParams {
            threshold_sigma: params.threshold_sigma,
            min_area,
            max_area,
        };

        let outcome = match source {
            ImageSource::Document(doc_id) => {
                match self.detect_via_document(doc_id, &resolved).await {
                    Ok(o) => o,
                    Err(e) => return Ok(tool_error!("{}", e)),
                }
            }
            ImageSource::Path(path) => match self.detect_via_path(path, &resolved).await {
                Ok(o) => o,
                Err(e) => return Ok(tool_error!("{}", e)),
            },
        };

        let stars_json: Vec<serde_json::Value> = outcome.stars.iter().map(star_to_json).collect();
        let star_count = u32::try_from(outcome.stars.len()).unwrap_or(u32::MAX);
        let saturated_star_count = u32::try_from(
            outcome
                .stars
                .iter()
                .filter(|s| s.saturated_pixel_count > 0)
                .count(),
        )
        .unwrap_or(u32::MAX);

        let payload = serde_json::json!({
            "stars": stars_json,
            "star_count": star_count,
            "saturated_star_count": saturated_star_count,
            "background_mean": outcome.background.mean,
            "background_stddev": outcome.background.stddev,
        });

        if let Some(doc_id) = params.document_id.as_deref() {
            if let Err(e) = self
                .image_cache
                .put_section(doc_id, "detected_stars", payload.clone())
                .await
            {
                debug!(error = %e, document_id = %doc_id, "failed to persist detected_stars section");
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }

    #[tool(
        description = "Per-star photometry and PSF metrics (HFR, FWHM, eccentricity, flux) on a captured image"
    )]
    pub(crate) async fn measure_stars(
        &self,
        Parameters(params): Parameters<MeasureStarsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source =
            match require_image_source(params.document_id.as_deref(), params.image_path.as_deref())
            {
                Ok(s) => s,
                Err(msg) => return Ok(tool_error!("{}", msg)),
            };
        let Some(min_area) = params.min_area else {
            return Ok(tool_error!("missing required parameter: min_area"));
        };
        let Some(max_area) = params.max_area else {
            return Ok(tool_error!("missing required parameter: max_area"));
        };
        if params.stamp_half_size == 0 {
            return Ok(tool_error!(
                "invalid parameter: stamp_half_size must be >= 1"
            ));
        }
        let resolved = ResolvedMeasureStarsParams {
            threshold_sigma: params.threshold_sigma,
            min_area,
            max_area,
            stamp_half_size: params.stamp_half_size,
        };

        let result = match source {
            ImageSource::Document(doc_id) => {
                match self.measure_stars_via_document(doc_id, &resolved).await {
                    Ok(r) => r,
                    Err(e) => return Ok(tool_error!("{}", e)),
                }
            }
            ImageSource::Path(path) => match self.measure_stars_via_path(path, &resolved).await {
                Ok(r) => r,
                Err(e) => return Ok(tool_error!("{}", e)),
            },
        };

        let payload = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);

        if let Some(doc_id) = params.document_id.as_deref() {
            if let Err(e) = self
                .image_cache
                .put_section(doc_id, "measured_stars", payload.clone())
                .await
            {
                debug!(error = %e, document_id = %doc_id, "failed to persist measured_stars section");
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }

    #[tool(
        description = "Median per-star signal-to-noise ratio via the CCD-equation approximation"
    )]
    pub(crate) async fn compute_snr(
        &self,
        Parameters(params): Parameters<ComputeSnrParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let source =
            match require_image_source(params.document_id.as_deref(), params.image_path.as_deref())
            {
                Ok(s) => s,
                Err(msg) => return Ok(tool_error!("{}", msg)),
            };
        let Some(min_area) = params.min_area else {
            return Ok(tool_error!("missing required parameter: min_area"));
        };
        let Some(max_area) = params.max_area else {
            return Ok(tool_error!("missing required parameter: max_area"));
        };
        let resolved = ResolvedDetectParams {
            threshold_sigma: params.threshold_sigma,
            min_area,
            max_area,
        };

        let result = match source {
            ImageSource::Document(doc_id) => match self.snr_via_document(doc_id, &resolved).await {
                Ok(r) => r,
                Err(e) => return Ok(tool_error!("{}", e)),
            },
            ImageSource::Path(path) => match self.snr_via_path(path, &resolved).await {
                Ok(r) => r,
                Err(e) => return Ok(tool_error!("{}", e)),
            },
        };

        let payload = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);

        if let Some(doc_id) = params.document_id.as_deref() {
            if let Err(e) = self
                .image_cache
                .put_section(doc_id, "snr", payload.clone())
                .await
            {
                debug!(error = %e, document_id = %doc_id, "failed to persist snr section");
            }
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn require_image_source_prefers_document_id_when_both_present() {
        let src = require_image_source(Some("doc-1"), Some("/tmp/img.fits")).unwrap();
        assert!(matches!(src, ImageSource::Document("doc-1")));
    }

    #[test]
    fn require_image_source_uses_path_when_only_path_present() {
        let src = require_image_source(None, Some("/tmp/img.fits")).unwrap();
        assert!(matches!(src, ImageSource::Path("/tmp/img.fits")));
    }

    #[test]
    fn require_image_source_uses_document_when_only_document_present() {
        let src = require_image_source(Some("doc-1"), None).unwrap();
        assert!(matches!(src, ImageSource::Document("doc-1")));
    }

    #[test]
    fn require_image_source_errors_when_neither_present() {
        let err = require_image_source(None, None).unwrap_err();
        assert_eq!(err, MISSING_SOURCE_ERROR);
    }
}
