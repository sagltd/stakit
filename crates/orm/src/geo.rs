//! First-class PostGIS (geospatial) support for the Postgres backend.
//!
//! PostGIS is a Postgres extension, so these types and functions target Postgres
//! only. Geometry values travel as their **(E)WKT text** (`POINT(1 2)`,
//! `SRID=4326;POINT(1 2)`) — no extra dependency — bound with a `::geometry` cast
//! and read back via `ST_AsText`, exactly mirroring how [`crate::vector`] uses
//! `::vector` + `::text`.
//!
//! Construct points with the ergonomic [`GeoPoint`]; reach for [`Geometry`] /
//! [`Geography`] when you have raw (E)WKT for any other shape. Filter with the
//! parameter-bound spatial predicates ([`st_dwithin`], [`st_intersects`],
//! [`st_contains`], [`st_within`]), project a distance with [`st_distance`], and
//! KNN-order with [`Select::nearest_geo`](crate::Select::nearest_geo) (`<->`).
//!
//! ```no_run
//! use stakit_orm::prelude::*;
//! use stakit_orm::geo::{GeoPoint, st_dwithin, st_distance};
//!
//! # async fn demo(db: Db) -> stakit_orm::Result<()> {
//! # #[derive(Table)] #[table(name="places")]
//! # struct Place { #[column(pk)] id: i64, #[column(sql_type="geometry(Point,4326)")] location: GeoPoint }
//! let here = GeoPoint::with_srid(13.405, 52.52, 4326);
//! // places within 1km, nearest first, with their distance
//! let near = db
//!     .select((Place::id, st_distance(Place::location, here)))
//!     .from::<Place>()
//!     .filter(st_dwithin(Place::location, here, 1000.0))
//!     .nearest_geo(Place::location, here)
//!     .limit(10)
//!     .all()
//!     .await?;
//! # let _ = near; Ok(()) }
//! ```

use crate::driver::Row;
use crate::error::{Error, Result};
use crate::expr::{Operand, Predicate};
use crate::projection::Projection;
use crate::schema::Col;
use crate::sql::SqlWriter;
use crate::value::{FromValue, ToValue, Value, ValueKind};

/// Mean Earth radius in metres — the conventional spherical haversine constant.
const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// A PostGIS geometry given as raw **(E)WKT** text and an optional SRID.
///
/// The general escape hatch for any shape (polygons, linestrings, …). For points
/// prefer the typed [`GeoPoint`]. Bound as `$N::geometry`; read back via
/// `ST_AsText`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    /// Well-known-text body, e.g. `POLYGON((0 0,1 0,1 1,0 0))`.
    pub wkt: String,
    /// Optional spatial reference id (e.g. `4326`), emitted as an `SRID=N;` prefix.
    pub srid: Option<i32>,
}

/// A PostGIS geography given as raw **(E)WKT** text and an optional SRID.
///
/// Same wire form as [`Geometry`] (it binds via the geometry path); use it to
/// document intent when a column is declared `geography(...)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geography {
    /// Well-known-text body.
    pub wkt: String,
    /// Optional spatial reference id (e.g. `4326`).
    pub srid: Option<i32>,
}

/// A typed 2D point (`x = lng`, `y = lat`) with an optional SRID — the
/// first-class geo column type and spatial-function argument.
///
/// Renders to WKT `POINT(lng lat)` (longitude first, the standard `x y` order),
/// or EWKT `SRID=4326;POINT(lng lat)` when a SRID is set.
///
/// The fields are stored as `lng`/`lat`, but the primary constructor
/// [`GeoPoint::new`] takes them in **(lat, lng)** GPS order — see its docs.
///
/// `serde` (de)serializes it as the object `{ "lat": .., "lng": .., "srid": .. }`
/// (the `srid` key is omitted when `None`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GeoPoint {
    /// Latitude (the WKT `y`).
    pub lat: f64,
    /// Longitude (the WKT `x`).
    pub lng: f64,
    /// Optional spatial reference id (e.g. `4326`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srid: Option<i32>,
}

/// A linear distance unit for [`GeoPoint::distance`] and the radius helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceUnit {
    /// Metres (the base unit).
    Meters,
    /// Kilometres (`m / 1000`).
    Kilometers,
    /// Statute miles (`m / 1609.344`).
    Miles,
    /// Nautical miles (`m / 1852`).
    NauticalMiles,
}

impl DistanceUnit {
    /// Convert a distance in metres into this unit.
    #[must_use]
    pub fn from_meters(self, meters: f64) -> f64 {
        match self {
            Self::Meters => meters,
            Self::Kilometers => meters / 1000.0,
            Self::Miles => meters / 1609.344,
            Self::NauticalMiles => meters / 1852.0,
        }
    }

    /// Convert a distance in this unit into metres.
    #[must_use]
    pub fn to_meters(self, value: f64) -> f64 {
        match self {
            Self::Meters => value,
            Self::Kilometers => value * 1000.0,
            Self::Miles => value * 1609.344,
            Self::NauticalMiles => value * 1852.0,
        }
    }
}

/// One coordinate in degrees / minutes / seconds with a hemisphere letter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dms {
    /// Whole degrees (always non-negative; sign lives in [`hemisphere`](Self::hemisphere)).
    pub degrees: i32,
    /// Whole arc-minutes (`0..60`).
    pub minutes: u32,
    /// Arc-seconds (`0.0..60.0`).
    pub seconds: f64,
    /// `N`/`S` for a latitude, `E`/`W` for a longitude.
    pub hemisphere: char,
}

impl Geometry {
    /// Wrap raw (E)WKT with no SRID prefix.
    #[must_use]
    pub fn new(wkt: impl Into<String>) -> Self {
        Self {
            wkt: wkt.into(),
            srid: None,
        }
    }

    /// Wrap raw WKT and emit an `SRID=<srid>;` prefix.
    #[must_use]
    pub fn with_srid(wkt: impl Into<String>, srid: i32) -> Self {
        Self {
            wkt: wkt.into(),
            srid: Some(srid),
        }
    }

    /// The (E)WKT literal this binds as (`SRID=N;<wkt>` when a SRID is set).
    #[must_use]
    pub fn to_ewkt(&self) -> String {
        with_srid_prefix(&self.wkt, self.srid)
    }
}

impl Geography {
    /// Wrap raw (E)WKT with no SRID prefix.
    #[must_use]
    pub fn new(wkt: impl Into<String>) -> Self {
        Self {
            wkt: wkt.into(),
            srid: None,
        }
    }

    /// Wrap raw WKT and emit an `SRID=<srid>;` prefix.
    #[must_use]
    pub fn with_srid(wkt: impl Into<String>, srid: i32) -> Self {
        Self {
            wkt: wkt.into(),
            srid: Some(srid),
        }
    }

    /// The (E)WKT literal this binds as (`SRID=N;<wkt>` when a SRID is set).
    #[must_use]
    pub fn to_ewkt(&self) -> String {
        with_srid_prefix(&self.wkt, self.srid)
    }
}

impl GeoPoint {
    /// A point from **(latitude, longitude)** — the human / GPS order you read off
    /// a map or phone (e.g. `48.8566, 2.3522` for Paris), with no SRID.
    ///
    /// NOTE: the argument order is `(lat, lng)`, but WKT is always
    /// `POINT(<lng> <lat>)` (longitude first = `x`). Swapping the two is the single
    /// most common GIS bug; this constructor takes the familiar GPS order so call
    /// sites read naturally, then renders the correct `x y` WKT. If you think in
    /// `x, y` use [`GeoPoint::from_lng_lat`] instead.
    #[must_use]
    pub const fn new(lat: f64, lng: f64) -> Self {
        Self {
            lng,
            lat,
            srid: None,
        }
    }

    /// A point from **(longitude, latitude)** — WKT/`x, y` order, with no SRID.
    #[must_use]
    pub const fn from_lng_lat(lng: f64, lat: f64) -> Self {
        Self {
            lng,
            lat,
            srid: None,
        }
    }

    /// A point from **(latitude, longitude)** tagged with `srid` (e.g. `4326` for
    /// WGS 84). Renders as `SRID=<srid>;POINT(lng lat)`.
    #[must_use]
    pub const fn with_srid(lat: f64, lng: f64, srid: i32) -> Self {
        Self {
            lng,
            lat,
            srid: Some(srid),
        }
    }

    /// Latitude (the WKT `y`).
    #[must_use]
    pub const fn lat(&self) -> f64 {
        self.lat
    }

    /// Longitude (the WKT `x`).
    #[must_use]
    pub const fn lng(&self) -> f64 {
        self.lng
    }

    /// The bare WKT body: `POINT(lng lat)` (longitude first, no SRID prefix).
    #[must_use]
    pub fn wkt(&self) -> String {
        format!("POINT({} {})", self.lng, self.lat)
    }

    /// The EWKT literal: `POINT(lng lat)`, or `SRID=<srid>;POINT(lng lat)` when a
    /// SRID is set.
    #[must_use]
    pub fn ewkt(&self) -> String {
        with_srid_prefix(&self.wkt(), self.srid)
    }

    /// `(lat, lng)` — GPS order.
    #[must_use]
    pub const fn as_lat_lng(&self) -> (f64, f64) {
        (self.lat, self.lng)
    }

    /// `(lng, lat)` — WKT/`x, y` order.
    #[must_use]
    pub const fn as_lng_lat(&self) -> (f64, f64) {
        (self.lng, self.lat)
    }

    /// A GeoJSON `Point`: `{"type":"Point","coordinates":[lng,lat]}` (GeoJSON
    /// coordinates are `[longitude, latitude]`).
    #[must_use]
    pub fn to_geojson(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "Point",
            "coordinates": [self.lng, self.lat],
        })
    }

    /// Parse a GeoJSON `Point` (`{"type":"Point","coordinates":[lng,lat]}`).
    ///
    /// # Errors
    /// Returns [`Error::Decode`] if `value` is not a 2-element GeoJSON `Point`.
    pub fn from_geojson(value: &serde_json::Value) -> Result<Self> {
        let invalid = || Error::Decode(format!("invalid GeoJSON point: {value}").into());
        if value.get("type").and_then(serde_json::Value::as_str) != Some("Point") {
            return Err(invalid());
        }
        let coords = value
            .get("coordinates")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(invalid)?;
        let [lng, lat] = coords.as_slice() else {
            return Err(invalid());
        };
        let lng = lng.as_f64().ok_or_else(invalid)?;
        let lat = lat.as_f64().ok_or_else(invalid)?;
        Ok(Self {
            lng,
            lat,
            srid: None,
        })
    }

    /// Degrees / minutes / seconds for `(latitude, longitude)`.
    #[must_use]
    pub fn to_dms(&self) -> (Dms, Dms) {
        (
            Dms::from_decimal(self.lat, 'N', 'S'),
            Dms::from_decimal(self.lng, 'E', 'W'),
        )
    }

    /// Build a point from `(latitude, longitude)` DMS coordinates.
    ///
    /// SRID is left `None`. (Hemisphere letters carry the sign; the `lat`/`lng`
    /// roles come from argument position, not the letters.)
    #[must_use]
    pub fn from_dms(lat: Dms, lng: Dms) -> Self {
        Self {
            lng: lng.to_decimal(),
            lat: lat.to_decimal(),
            srid: None,
        }
    }

    /// Great-circle distance to `other` in metres (haversine, spherical Earth).
    ///
    /// SRID is ignored — both points are treated as WGS 84 lat/lng degrees.
    #[must_use]
    pub fn haversine_meters(&self, other: &Self) -> f64 {
        let (lat1, lng1) = (self.lat.to_radians(), self.lng.to_radians());
        let (lat2, lng2) = (other.lat.to_radians(), other.lng.to_radians());
        let (d_lat, d_lng) = (lat2 - lat1, lng2 - lng1);
        let half_lat = (d_lat / 2.0).sin().powi(2);
        let half_lng = (d_lng / 2.0).sin().powi(2);
        let a = (lat1.cos() * lat2.cos()).mul_add(half_lng, half_lat);
        2.0 * EARTH_RADIUS_M * a.sqrt().asin()
    }

    /// Great-circle distance to `other`, expressed in `unit` (built on
    /// [`haversine_meters`](Self::haversine_meters)).
    #[must_use]
    pub fn distance(&self, other: &Self, unit: DistanceUnit) -> f64 {
        unit.from_meters(self.haversine_meters(other))
    }

    /// Initial compass bearing from this point toward `other`, in degrees
    /// `[0, 360)` (0 = north, 90 = east).
    #[must_use]
    pub fn bearing(&self, other: &Self) -> f64 {
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let d_lng = (other.lng - self.lng).to_radians();
        let y = d_lng.sin() * lat2.cos();
        let x = (lat1.sin() * lat2.cos()).mul_add(-d_lng.cos(), lat1.cos() * lat2.sin());
        let bearing = y.atan2(x).to_degrees();
        (bearing + 360.0) % 360.0
    }

    /// The point reached by travelling `distance` (in `unit`) along `bearing_deg`
    /// (compass degrees) from here. SRID is inherited.
    #[must_use]
    pub fn destination(&self, bearing_deg: f64, distance: f64, unit: DistanceUnit) -> Self {
        let angular = unit.to_meters(distance) / EARTH_RADIUS_M;
        let bearing = bearing_deg.to_radians();
        let lat1 = self.lat.to_radians();
        let lng1 = self.lng.to_radians();
        let lat2 = (lat1.cos() * angular.sin())
            .mul_add(bearing.cos(), lat1.sin() * angular.cos())
            .asin();
        let lng2 = lng1
            + (bearing.sin() * angular.sin() * lat1.cos())
                .atan2(lat1.sin().mul_add(-lat2.sin(), angular.cos()));
        Self {
            lat: lat2.to_degrees(),
            lng: lng2.to_degrees(),
            srid: self.srid,
        }
    }

    /// The geographic midpoint between this point and `other`. SRID is inherited
    /// from `self`.
    #[must_use]
    pub fn midpoint(&self, other: &Self) -> Self {
        let lat1 = self.lat.to_radians();
        let lng1 = self.lng.to_radians();
        let lat2 = other.lat.to_radians();
        let d_lng = (other.lng - self.lng).to_radians();
        let bx = lat2.cos() * d_lng.cos();
        let by = lat2.cos() * d_lng.sin();
        let lat_mid = (lat1.sin() + lat2.sin()).atan2((lat1.cos() + bx).hypot(by));
        let lng_mid = lng1 + by.atan2(lat1.cos() + bx);
        Self {
            lat: lat_mid.to_degrees(),
            lng: lng_mid.to_degrees(),
            srid: self.srid,
        }
    }

    /// Whether `other` lies within `radius` (in `unit`) of this point — a
    /// client-side radius check (`distance <= radius`).
    #[must_use]
    pub fn within(&self, other: &Self, radius: f64, unit: DistanceUnit) -> bool {
        self.haversine_meters(other) <= unit.to_meters(radius)
    }

    /// A `(min, max)` lat/lng bounding box covering a circle of `radius` (in
    /// `unit`) around this point. Useful as a cheap, index-friendly pre-filter
    /// before a precise `ST_DWithin`. SRID is inherited.
    #[must_use]
    pub fn bounding_box(&self, radius: f64, unit: DistanceUnit) -> (Self, Self) {
        let radius_m = unit.to_meters(radius);
        let lat_delta = (radius_m / EARTH_RADIUS_M).to_degrees();
        // Longitude degrees shrink toward the poles; guard the cosine near ±90°.
        let cos_lat = self.lat.to_radians().cos().abs().max(1e-12);
        let lng_delta = (radius_m / (EARTH_RADIUS_M * cos_lat)).to_degrees();
        let min = Self {
            lat: self.lat - lat_delta,
            lng: self.lng - lng_delta,
            srid: self.srid,
        };
        let max = Self {
            lat: self.lat + lat_delta,
            lng: self.lng + lng_delta,
            srid: self.srid,
        };
        (min, max)
    }

    /// Whether the coordinates are in range (`lat ∈ [-90, 90]`,
    /// `lng ∈ [-180, 180]`).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        (-90.0..=90.0).contains(&self.lat) && (-180.0..=180.0).contains(&self.lng)
    }

    /// Validating constructor from `(latitude, longitude)` (GPS order).
    ///
    /// # Errors
    /// Returns [`Error::Decode`] if `lat`/`lng` are out of range (see
    /// [`is_valid`](Self::is_valid)). [`new`](Self::new) is the infallible variant
    /// (it does not range-check).
    pub fn try_new(lat: f64, lng: f64) -> Result<Self> {
        let point = Self::new(lat, lng);
        if point.is_valid() {
            Ok(point)
        } else {
            Err(Error::Decode(
                format!("coordinates out of range: lat={lat}, lng={lng}").into(),
            ))
        }
    }
}

/// `(lat, lng)` — GPS order — into a point with no SRID.
impl From<(f64, f64)> for GeoPoint {
    fn from((lat, lng): (f64, f64)) -> Self {
        Self::new(lat, lng)
    }
}

/// A point into `(lat, lng)` — GPS order.
impl From<GeoPoint> for (f64, f64) {
    fn from(point: GeoPoint) -> Self {
        (point.lat, point.lng)
    }
}

impl Dms {
    /// Decompose a signed decimal degree into DMS, choosing `positive`/`negative`
    /// as the hemisphere letter.
    #[must_use]
    pub fn from_decimal(decimal: f64, positive: char, negative: char) -> Self {
        let hemisphere = if decimal < 0.0 { negative } else { positive };
        let abs = decimal.abs();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "abs degrees fit i32 for any valid lat/lng; truncation toward zero is the intended floor"
        )]
        let degrees = abs.trunc() as i32;
        let minutes_full = (abs - abs.trunc()) * 60.0;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "minutes_full is in 0..60 and non-negative; floor to whole minutes"
        )]
        let minutes = minutes_full.trunc() as u32;
        let seconds = (minutes_full - minutes_full.trunc()) * 60.0;
        Self {
            degrees,
            minutes,
            seconds,
            hemisphere,
        }
    }

    /// Recombine into a signed decimal degree (negative for `S`/`W`).
    #[must_use]
    pub fn to_decimal(&self) -> f64 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "degrees/minutes are small whole numbers; f64 represents them exactly"
        )]
        let magnitude = f64::from(self.degrees) + f64::from(self.minutes) / 60.0 + self.seconds / 3600.0;
        if matches!(self.hemisphere, 'S' | 'W' | 's' | 'w') {
            -magnitude
        } else {
            magnitude
        }
    }
}

/// Prepend an `SRID=N;` prefix to `wkt` when `srid` is set.
fn with_srid_prefix(wkt: &str, srid: Option<i32>) -> String {
    srid.map_or_else(|| wkt.to_owned(), |srid| format!("SRID={srid};{wkt}"))
}

/// Split an optional `SRID=N;` prefix off an (E)WKT string, returning
/// `(srid, body)`.
fn split_srid(text: &str) -> (Option<i32>, &str) {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("SRID=").or_else(|| trimmed.strip_prefix("srid=")) {
        if let Some((digits, body)) = rest.split_once(';') {
            if let Ok(srid) = digits.trim().parse::<i32>() {
                return (Some(srid), body.trim());
            }
        }
    }
    (None, trimmed)
}

/// Parse a `POINT(x y)` / `SRID=N;POINT(x y)` body into `(lng, lat)`.
///
/// # Errors
/// Returns [`Error::Decode`] if the text is not a 2D WKT point.
fn parse_point(text: &str) -> Result<(f64, f64, Option<i32>)> {
    let (srid, body) = split_srid(text);
    let upper = body.trim();
    let inner = upper
        .strip_prefix("POINT")
        .or_else(|| upper.strip_prefix("point"))
        .map(str::trim)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| Error::Decode(format!("invalid WKT point: {text:?}").into()))?;
    let mut parts = inner.split_whitespace();
    let lng = parts
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| Error::Decode(format!("invalid WKT point: {text:?}").into()))?;
    let lat = parts
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| Error::Decode(format!("invalid WKT point: {text:?}").into()))?;
    Ok((lng, lat, srid))
}

impl ToValue for Geometry {
    fn to_value(self) -> Value {
        Value::Geo {
            wkt: self.wkt,
            srid: self.srid,
        }
    }
}

impl ToValue for Geography {
    fn to_value(self) -> Value {
        Value::Geo {
            wkt: self.wkt,
            srid: self.srid,
        }
    }
}

impl ToValue for GeoPoint {
    fn to_value(self) -> Value {
        Value::Geo {
            // Bare WKT body — the SRID stays a first-class field.
            wkt: self.wkt(),
            srid: self.srid,
        }
    }
}

impl FromValue for Geometry {
    const KIND: ValueKind = ValueKind::Geo;
    fn from_value(value: Value) -> Result<Self> {
        let (wkt, srid) = geo_parts(value)?;
        Ok(Self { wkt, srid })
    }
}

impl FromValue for Geography {
    const KIND: ValueKind = ValueKind::Geo;
    fn from_value(value: Value) -> Result<Self> {
        let (wkt, srid) = geo_parts(value)?;
        Ok(Self { wkt, srid })
    }
}

impl FromValue for GeoPoint {
    const KIND: ValueKind = ValueKind::Geo;
    fn from_value(value: Value) -> Result<Self> {
        let (wkt, srid) = geo_parts(value)?;
        let (lng, lat, body_srid) = parse_point(&wkt)?;
        Ok(Self {
            lng,
            lat,
            srid: srid.or(body_srid),
        })
    }
}

/// Decode a geo cell into `(bare_wkt, srid)`. Accepts `Value::Geo` (its `srid`
/// field wins) and `Value::Text` from `ST_AsText` (which drops the SRID, leaving
/// it `None` — read SRID separately, e.g. via `ST_SRID`, if you need it).
/// Defensively strips an `SRID=N;` prefix should one appear in the text.
fn geo_parts(value: Value) -> Result<(String, Option<i32>)> {
    let (text, field_srid) = match value {
        Value::Geo { wkt, srid } => (wkt, srid),
        Value::Text(text) => (text, None),
        other => {
            return Err(Error::Decode(
                format!("expected geometry, got {other:?}").into(),
            ));
        }
    };
    let (prefix_srid, body) = split_srid(&text);
    Ok((body.to_owned(), field_srid.or(prefix_srid)))
}

/// `IntoExpr` so a geo value can be the RHS of `eq` etc. against a geo column.
macro_rules! geo_into_expr {
    ($ty:ty) => {
        impl crate::expr::IntoExpr<$ty> for $ty {
            fn into_operand(self) -> Operand {
                Operand::Value(self.to_value())
            }
        }
        impl crate::expr::IntoExpr<Option<$ty>> for $ty {
            fn into_operand(self) -> Operand {
                Operand::Value(self.to_value())
            }
        }
    };
}

geo_into_expr!(Geometry);
geo_into_expr!(Geography);
geo_into_expr!(GeoPoint);

/// A value usable as the geometry argument to a spatial function ([`GeoPoint`],
/// [`Geometry`], or [`Geography`]).
pub trait IntoGeo {
    /// Convert into the bound geometry [`Value`].
    fn into_geo_value(self) -> Value;
}

impl<T: ToValue> IntoGeo for T {
    fn into_geo_value(self) -> Value {
        self.to_value()
    }
}

/// `ST_DWithin("t"."col", $N::geometry, $M)` — rows whose geometry is within
/// `distance` of `geom` (the value and the distance are both bound).
#[must_use]
pub fn st_dwithin<T, Ty>(column: Col<T, Ty>, geom: impl IntoGeo, distance: f64) -> Predicate {
    Predicate::Spatial {
        func: "st_dwithin",
        table: column.table,
        name: column.name,
        geom: geom.into_geo_value(),
        distance: Some(distance),
    }
}

/// `ST_Intersects("t"."col", $N::geometry)`.
#[must_use]
pub fn st_intersects<T, Ty>(column: Col<T, Ty>, geom: impl IntoGeo) -> Predicate {
    spatial("st_intersects", column.table, column.name, geom)
}

/// `ST_Contains("t"."col", $N::geometry)` — the column's geometry contains `geom`.
#[must_use]
pub fn st_contains<T, Ty>(column: Col<T, Ty>, geom: impl IntoGeo) -> Predicate {
    spatial("st_contains", column.table, column.name, geom)
}

/// `ST_Within("t"."col", $N::geometry)` — the column's geometry is within `geom`.
#[must_use]
pub fn st_within<T, Ty>(column: Col<T, Ty>, geom: impl IntoGeo) -> Predicate {
    spatial("st_within", column.table, column.name, geom)
}

fn spatial(
    func: &'static str,
    table: &'static str,
    name: &'static str,
    geom: impl IntoGeo,
) -> Predicate {
    Predicate::Spatial {
        func,
        table,
        name,
        geom: geom.into_geo_value(),
        distance: None,
    }
}

/// A **selectable** `ST_Distance(col, geom)` projection (output `f64`).
///
/// Put it in a `select(...)` tuple (or a `#[derive(Row)]` `#[from(..)]`) to read
/// the distance back alongside the rows — the geo analogue of
/// [`crate::vector::distance`]. Renders `ST_Distance("t"."col", $N::geometry)`.
pub struct GeoDistance {
    table: &'static str,
    name: &'static str,
    geom: Value,
}

/// Build a selectable [`GeoDistance`] for `column` against `geom`.
#[must_use]
pub fn st_distance<T, Ty>(column: Col<T, Ty>, geom: impl IntoGeo) -> GeoDistance {
    GeoDistance {
        table: column.table,
        name: column.name,
        geom: geom.into_geo_value(),
    }
}

impl Projection for GeoDistance {
    type Output = f64;
    fn arity(&self) -> usize {
        1
    }
    fn write_columns(&self, out: &mut SqlWriter) -> Result<()> {
        out.push("st_distance(");
        out.push_qualified(self.table, self.name)?;
        out.push(", ");
        out.push_bind(self.geom.clone());
        out.push(")");
        Ok(())
    }
    fn decode(&self, row: &dyn Row, start: usize) -> Result<f64> {
        crate::driver::decode_cell(row, start)
    }
}

#[cfg(test)]
mod tests {
    use super::{DistanceUnit, Dms, GeoPoint, Geography, Geometry, parse_point, split_srid};
    use crate::value::{FromValue, ToValue, Value};

    #[test]
    fn new_takes_lat_lng_but_renders_lng_first() {
        // Paris: lat 48.85, lng 2.35. WKT must be POINT(lng lat) — longitude first.
        let paris = GeoPoint::new(48.85, 2.35);
        assert_eq!(paris.wkt(), "POINT(2.35 48.85)");
        assert_eq!(paris.lat(), 48.85);
        assert_eq!(paris.lng(), 2.35);
    }

    #[test]
    fn from_lng_lat_uses_xy_order() {
        assert_eq!(GeoPoint::from_lng_lat(2.35, 48.85).wkt(), "POINT(2.35 48.85)");
    }

    #[test]
    fn point_renders_plain_wkt() {
        assert_eq!(GeoPoint::from_lng_lat(1.5, -2.0).wkt(), "POINT(1.5 -2)");
    }

    #[test]
    fn point_renders_ewkt_with_srid() {
        // with_srid is (lat, lng, srid): lat 52.52, lng 13.405.
        assert_eq!(
            GeoPoint::with_srid(52.52, 13.405, 4326).ewkt(),
            "SRID=4326;POINT(13.405 52.52)"
        );
    }

    #[test]
    fn point_value_keeps_srid_as_a_field() {
        // The WKT body stays bare; the SRID is a first-class field on the Value.
        assert_eq!(
            GeoPoint::with_srid(2.0, 1.0, 4326).to_value(),
            Value::Geo {
                wkt: "POINT(1 2)".to_owned(),
                srid: Some(4326),
            }
        );
    }

    #[test]
    fn point_round_trips_through_value() {
        let p = GeoPoint::with_srid(2.0, 1.0, 4326);
        assert_eq!(GeoPoint::from_value(p.to_value()).unwrap(), p);
    }

    #[test]
    fn point_parses_back_from_astext() {
        // `ST_AsText` drops the SRID; the body still parses (srid left None).
        let got = GeoPoint::from_value(Value::Text("POINT(1 2)".to_owned())).unwrap();
        assert_eq!(got, GeoPoint::from_lng_lat(1.0, 2.0));
    }

    #[test]
    fn lat_lng_and_lng_lat_accessors() {
        let p = GeoPoint::new(48.85, 2.35);
        assert_eq!(p.as_lat_lng(), (48.85, 2.35));
        assert_eq!(p.as_lng_lat(), (2.35, 48.85));
    }

    #[test]
    fn geojson_round_trips_lng_lat() {
        let p = GeoPoint::new(48.85, 2.35);
        let json = p.to_geojson();
        assert_eq!(json["type"], "Point");
        assert_eq!(json["coordinates"][0], 2.35); // lng first
        assert_eq!(json["coordinates"][1], 48.85); // lat second
        assert_eq!(GeoPoint::from_geojson(&json).unwrap(), p);
    }

    #[test]
    fn geojson_rejects_non_point() {
        let bad = serde_json::json!({"type": "LineString", "coordinates": [[0, 0]]});
        assert!(GeoPoint::from_geojson(&bad).is_err());
    }

    #[test]
    fn dms_decomposes_paris_latitude() {
        // 48.8566° ≈ 48°51'23.76"N
        let dms = Dms::from_decimal(48.8566, 'N', 'S');
        assert_eq!(dms.degrees, 48);
        assert_eq!(dms.minutes, 51);
        assert!((dms.seconds - 23.76).abs() < 0.01, "got {}", dms.seconds);
        assert_eq!(dms.hemisphere, 'N');
    }

    #[test]
    fn dms_round_trips_decimal() {
        let p = GeoPoint::new(48.8566, 2.3522);
        let (lat, lng) = p.to_dms();
        let back = GeoPoint::from_dms(lat, lng);
        assert!((back.lat - p.lat).abs() < 1e-9, "lat {} vs {}", back.lat, p.lat);
        assert!((back.lng - p.lng).abs() < 1e-9, "lng {} vs {}", back.lng, p.lng);
    }

    #[test]
    fn dms_southern_western_hemispheres_are_negative() {
        let south = Dms::from_decimal(-33.8688, 'N', 'S');
        assert_eq!(south.hemisphere, 'S');
        assert!((south.to_decimal() - -33.8688).abs() < 1e-9);
    }

    #[test]
    fn haversine_paris_to_london_is_about_343km() {
        let paris = GeoPoint::new(48.8566, 2.3522);
        let london = GeoPoint::new(51.5074, -0.1278);
        let meters = paris.haversine_meters(&london);
        // ~343 km; assert within 1%.
        assert!(
            (meters - 343_000.0).abs() < 343_000.0 * 0.01,
            "got {meters} m"
        );
    }

    #[test]
    fn distance_unit_conversions() {
        assert!((DistanceUnit::Kilometers.from_meters(2000.0) - 2.0).abs() < 1e-9);
        assert!((DistanceUnit::Miles.from_meters(1609.344) - 1.0).abs() < 1e-9);
        assert!((DistanceUnit::NauticalMiles.from_meters(1852.0) - 1.0).abs() < 1e-9);
        assert!((DistanceUnit::Kilometers.to_meters(3.0) - 3000.0).abs() < 1e-9);
    }

    #[test]
    fn distance_in_km_and_miles() {
        let paris = GeoPoint::new(48.8566, 2.3522);
        let london = GeoPoint::new(51.5074, -0.1278);
        let km = paris.distance(&london, DistanceUnit::Kilometers);
        assert!((km - 343.0).abs() < 343.0 * 0.01, "got {km} km");
        let mi = paris.distance(&london, DistanceUnit::Miles);
        assert!((mi - 213.0).abs() < 213.0 * 0.01, "got {mi} mi");
    }

    #[test]
    fn bearing_is_zero_going_north_and_ninety_going_east() {
        let origin = GeoPoint::new(0.0, 0.0);
        let north = GeoPoint::new(1.0, 0.0);
        let east = GeoPoint::new(0.0, 1.0);
        assert!(origin.bearing(&north).abs() < 0.5, "{}", origin.bearing(&north));
        assert!(
            (origin.bearing(&east) - 90.0).abs() < 0.5,
            "{}",
            origin.bearing(&east)
        );
    }

    #[test]
    fn destination_round_trips_distance() {
        let start = GeoPoint::new(48.8566, 2.3522);
        let moved = start.destination(75.0, 10.0, DistanceUnit::Kilometers);
        let back = moved.distance(&start, DistanceUnit::Kilometers);
        assert!((back - 10.0).abs() < 0.05, "got {back} km");
    }

    #[test]
    fn midpoint_is_roughly_centered() {
        let a = GeoPoint::new(0.0, 0.0);
        let b = GeoPoint::new(0.0, 10.0);
        let mid = a.midpoint(&b);
        assert!((mid.lat).abs() < 1e-6, "lat {}", mid.lat);
        assert!((mid.lng - 5.0).abs() < 1e-6, "lng {}", mid.lng);
    }

    #[test]
    fn within_radius_check() {
        let a = GeoPoint::new(48.8566, 2.3522);
        let near = a.destination(0.0, 500.0, DistanceUnit::Meters);
        assert!(a.within(&near, 1.0, DistanceUnit::Kilometers));
        assert!(!a.within(&near, 100.0, DistanceUnit::Meters));
    }

    #[test]
    fn bounding_box_contains_point_and_spans_radius() {
        let p = GeoPoint::new(48.8566, 2.3522);
        let (min, max) = p.bounding_box(10.0, DistanceUnit::Kilometers);
        assert!(min.lat <= p.lat && p.lat <= max.lat);
        assert!(min.lng <= p.lng && p.lng <= max.lng);
        // The lat span is ~2 * 10km in degrees (1° lat ≈ 111 km).
        let lat_span_km = (max.lat - min.lat) * 111.0;
        assert!((lat_span_km - 20.0).abs() < 2.0, "got {lat_span_km} km");
    }

    #[test]
    fn validation_rejects_out_of_range() {
        assert!(GeoPoint::new(48.0, 2.0).is_valid());
        assert!(!GeoPoint::new(91.0, 2.0).is_valid());
        assert!(GeoPoint::try_new(48.0, 2.0).is_ok());
        assert!(GeoPoint::try_new(0.0, 200.0).is_err());
    }

    #[test]
    fn tuple_conversions_use_lat_lng() {
        let p = GeoPoint::from((48.85, 2.35));
        assert_eq!(p.as_lat_lng(), (48.85, 2.35));
        let (lat, lng): (f64, f64) = p.into();
        assert_eq!((lat, lng), (48.85, 2.35));
    }

    #[test]
    fn serde_round_trips_as_lat_lng_object() {
        let p = GeoPoint::with_srid(48.85, 2.35, 4326);
        let json = serde_json::to_value(p).unwrap();
        assert_eq!(json["lat"], 48.85);
        assert_eq!(json["lng"], 2.35);
        assert_eq!(json["srid"], 4326);
        let back: GeoPoint = serde_json::from_value(json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn geometry_round_trips() {
        let g = Geometry::with_srid("POLYGON((0 0,1 0,1 1,0 0))", 4326);
        assert_eq!(g.to_ewkt(), "SRID=4326;POLYGON((0 0,1 0,1 1,0 0))");
        let back = Geometry::from_value(g.clone().to_value()).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn geography_round_trips() {
        let g = Geography::new("LINESTRING(0 0,1 1)");
        assert_eq!(Geography::from_value(g.clone().to_value()).unwrap(), g);
    }

    #[test]
    fn srid_prefix_splits() {
        assert_eq!(split_srid("SRID=4326;POINT(1 2)"), (Some(4326), "POINT(1 2)"));
        assert_eq!(split_srid("POINT(1 2)"), (None, "POINT(1 2)"));
    }

    #[test]
    fn malformed_point_is_error() {
        assert!(parse_point("NOTAPOINT").is_err());
        assert!(parse_point("POINT(1)").is_err());
    }
}
