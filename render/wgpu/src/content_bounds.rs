/// Axis-aligned bounds of the content actually drawn into a render target,
/// expressed in that target's pixel space.
///
/// Command matrices reach the vertex shader already in pixels (`add_to_current`
/// converts `tx`/`ty` with `to_pixels`, and the globals' view matrix is what
/// maps pixels to NDC), so bounds accumulated here can be handed to
/// `set_scissor_rect` without further conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContentBounds {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl Default for ContentBounds {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl ContentBounds {
    /// Covers nothing. Unioning with this is a no-op.
    pub const EMPTY: Self = Self {
        min_x: f32::INFINITY,
        min_y: f32::INFINITY,
        max_x: f32::NEG_INFINITY,
        max_y: f32::NEG_INFINITY,
    };

    /// Covers everything, for content whose extent we cannot bound.
    pub const UNBOUNDED: Self = Self {
        min_x: f32::NEG_INFINITY,
        min_y: f32::NEG_INFINITY,
        max_x: f32::INFINITY,
        max_y: f32::INFINITY,
    };

    /// The unit square, which is the local geometry of every quad-based draw
    /// (`Quad::vertices_pos` runs (0,0)-(1,1)).
    pub const UNIT: Self = Self {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };

    pub fn is_empty(&self) -> bool {
        !(self.max_x > self.min_x && self.max_y > self.min_y)
    }

    pub fn union(&mut self, other: ContentBounds) {
        if other.is_empty() {
            return;
        }
        self.min_x = self.min_x.min(other.min_x);
        self.min_y = self.min_y.min(other.min_y);
        self.max_x = self.max_x.max(other.max_x);
        self.max_y = self.max_y.max(other.max_y);
    }

    /// Grows to cover `local` mapped through `matrix`, which must be the same
    /// pixel-space matrix handed to the vertex shader.
    ///
    /// The shader computes `world_matrix * vec4(x, y, 1, 1)` against a
    /// column-major matrix built as `[[a, b, ..], [c, d, ..], .., [tx, ty, ..]]`,
    /// which works out to `x' = a*x + c*y + tx`, `y' = b*x + d*y + ty`. All four
    /// corners are mapped, so rotation and skew stay covered.
    pub fn union_transformed(&mut self, matrix: &ruffle_render::matrix::Matrix, local: Self) {
        if local.is_empty() {
            return;
        }
        if local == Self::UNBOUNDED {
            *self = Self::UNBOUNDED;
            return;
        }

        let tx = matrix.tx.to_pixels() as f32;
        let ty = matrix.ty.to_pixels() as f32;
        let corners = [
            (local.min_x, local.min_y),
            (local.max_x, local.min_y),
            (local.max_x, local.max_y),
            (local.min_x, local.max_y),
        ];

        for (x, y) in corners {
            let px = matrix.a * x + matrix.c * y + tx;
            let py = matrix.b * x + matrix.d * y + ty;
            // A degenerate transform (the AQW "incredibly large object" case
            // among them) must not poison the bounds into garbage; treat any
            // non-finite corner as unbounded and let the caller fall back to a
            // full-surface pass.
            if !px.is_finite() || !py.is_finite() {
                *self = Self::UNBOUNDED;
                return;
            }
            self.min_x = self.min_x.min(px);
            self.min_y = self.min_y.min(py);
            self.max_x = self.max_x.max(px);
            self.max_y = self.max_y.max(py);
        }
    }

    /// Grows to cover a rect already in this target's pixel space.
    ///
    /// Nested command lists (blends, alpha masks) render into a target of the
    /// same size and coordinate system as their parent, so their accumulated
    /// bounds carry over untransformed.
    pub fn union_pixels(&mut self, other: ContentBounds) {
        self.union(other);
    }

    /// Shifts these bounds by `(dx, dy)`, to move between the coordinate space
    /// commands are written in and a target covering only part of it.
    pub fn translated(self, dx: f32, dy: f32) -> Self {
        if self.is_empty() || self == Self::UNBOUNDED {
            return self;
        }
        Self {
            min_x: self.min_x + dx,
            min_y: self.min_y + dy,
            max_x: self.max_x + dx,
            max_y: self.max_y + dy,
        }
    }

    /// The rect this covers, clipped to a `width` x `height` surface and then
    /// grown so both dimensions are pool bucket sizes.
    ///
    /// Snapping the *bounds* rather than only the allocation keeps the logical
    /// and allocated sizes equal, which is what lets a blend pass address the
    /// source with the plain unit-quad coordinate instead of carrying a
    /// separate scale for the padding. The origin is pulled back toward zero
    /// when growing would otherwise run off the far edge.
    pub fn to_snapped_rect(
        self,
        width: u32,
        height: u32,
        bucket: impl Fn(u32) -> u32,
    ) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = self.to_scissor(width, height, 1)?;

        let snap = |origin: u32, extent: u32, limit: u32| {
            let extent = bucket(extent).min(limit);
            // Growing past the edge shifts the window back instead of
            // clipping, so the content stays inside it.
            let origin = origin.min(limit - extent);
            (origin, extent)
        };

        let (x, w) = snap(x, w, width);
        let (y, h) = snap(y, h, height);
        Some((x, y, w, h))
    }

    pub fn from_points(points: impl IntoIterator<Item = (f32, f32)>) -> Self {
        let mut bounds = Self::EMPTY;
        for (x, y) in points {
            if !x.is_finite() || !y.is_finite() {
                return Self::UNBOUNDED;
            }
            bounds.min_x = bounds.min_x.min(x);
            bounds.min_y = bounds.min_y.min(y);
            bounds.max_x = bounds.max_x.max(x);
            bounds.max_y = bounds.max_y.max(y);
        }
        bounds
    }

    /// The pixel area this covers once clipped to a `width` x `height` target,
    /// for measuring how much of a full-surface pass is actually live.
    pub fn clipped_area(&self, width: u32, height: u32) -> u64 {
        match self.to_scissor(width, height, 0) {
            Some((_, _, w, h)) => w as u64 * h as u64,
            None => 0,
        }
    }

    /// The scissor rect covering this content inside a `width` x `height`
    /// attachment, grown by `pad` pixels, or `None` when nothing is covered.
    ///
    /// `pad` is slack against rounding in the NDC-to-UV round trip; the blend
    /// shaders sample 1:1 through a `clamp_nearest` sampler, so a pixel or two
    /// is plenty.
    pub fn to_scissor(self, width: u32, height: u32, pad: u32) -> Option<(u32, u32, u32, u32)> {
        if self.is_empty() || width == 0 || height == 0 {
            return None;
        }

        let pad = pad as f32;
        let min_x = (self.min_x - pad).floor().max(0.0);
        let min_y = (self.min_y - pad).floor().max(0.0);
        let max_x = (self.max_x + pad).ceil().min(width as f32);
        let max_y = (self.max_y + pad).ceil().min(height as f32);

        if !(max_x > min_x && max_y > min_y) {
            return None;
        }

        Some((
            min_x as u32,
            min_y as u32,
            (max_x - min_x) as u32,
            (max_y - min_y) as u32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruffle_render::matrix::Matrix;
    use swf::Twips;

    #[test]
    fn empty_unions_away() {
        let mut bounds = ContentBounds::EMPTY;
        assert!(bounds.is_empty());
        bounds.union(ContentBounds::EMPTY);
        assert!(bounds.is_empty());
        assert_eq!(bounds.to_scissor(100, 100, 0), None);
    }

    #[test]
    fn unit_quad_scaled_and_translated() {
        // What `render_bitmap` builds: the texture size folded into the matrix.
        let matrix = Matrix::create_box(
            40.0,
            20.0,
            Twips::from_pixels(10.0),
            Twips::from_pixels(5.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(bounds.to_scissor(1600, 841, 0), Some((10, 5, 40, 20)));
    }

    #[test]
    fn rotation_covers_the_swept_corners() {
        // A 100px square rotated 45 degrees sweeps sqrt(2) wider than its side,
        // which only the full four-corner mapping catches. Translated clear of
        // the origin so the clamp to the attachment cannot mask a short edge.
        let matrix = Matrix::create_box_with_rotation(
            100.0,
            100.0,
            std::f32::consts::FRAC_PI_4,
            Twips::from_pixels(400.0),
            Twips::from_pixels(400.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        let (_, _, w, h) = bounds.to_scissor(1600, 841, 0).expect("covers something");
        assert_eq!((w, h), (142, 142), "expected the ~141.4px diagonal span");
    }

    #[test]
    fn clamps_to_the_attachment() {
        let matrix = Matrix::create_box(
            4000.0,
            4000.0,
            Twips::from_pixels(-500.0),
            Twips::from_pixels(-500.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(bounds.to_scissor(1600, 841, 0), Some((0, 0, 1600, 841)));
    }

    #[test]
    fn fully_offscreen_content_scissors_to_nothing() {
        let matrix = Matrix::create_box(
            10.0,
            10.0,
            Twips::from_pixels(-900.0),
            Twips::from_pixels(-900.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(bounds.to_scissor(1600, 841, 0), None);
    }

    #[test]
    fn padding_grows_but_stays_inside() {
        let matrix =
            Matrix::create_box(10.0, 10.0, Twips::from_pixels(0.0), Twips::from_pixels(0.0));
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(bounds.to_scissor(1600, 841, 2), Some((0, 0, 12, 12)));
    }

    #[test]
    fn astronomically_large_transform_clamps_to_the_surface() {
        // The AQW "incredibly large object" shape (instance34380 at 6711355px
        // and worse). Still finite, so it stays a real rect -- it just covers
        // everything, which is the conservative answer.
        let matrix = Matrix::create_box(
            f32::MAX,
            f32::MAX,
            Twips::from_pixels(0.0),
            Twips::from_pixels(0.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(bounds.to_scissor(1600, 841, 0), Some((0, 0, 1600, 841)));
    }

    #[test]
    fn non_finite_transform_is_unbounded() {
        // inf * 0 is NaN, so a non-finite matrix must be caught rather than
        // compared into the bounds, where NaN loses every min/max.
        let matrix = Matrix::create_box(
            f32::INFINITY,
            f32::INFINITY,
            Twips::from_pixels(0.0),
            Twips::from_pixels(0.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(bounds, ContentBounds::UNBOUNDED);
        assert_eq!(bounds.to_scissor(1600, 841, 0), Some((0, 0, 1600, 841)));
    }

    #[test]
    fn unbounded_local_stays_unbounded() {
        let matrix =
            Matrix::create_box(1.0, 1.0, Twips::from_pixels(10.0), Twips::from_pixels(10.0));
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNBOUNDED);
        assert_eq!(bounds, ContentBounds::UNBOUNDED);
    }

    /// Same rounding the texture pool keys on.
    fn bucket(dim: u32) -> u32 {
        crate::buffer_pool::quantize_pool_dimension(dim)
    }

    #[test]
    fn snapped_rect_grows_to_a_bucket_and_still_covers() {
        let matrix = Matrix::create_box(
            50.0,
            50.0,
            Twips::from_pixels(100.0),
            Twips::from_pixels(100.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        // 50px of content plus a pixel of slack each side rounds to 64.
        let (x, y, w, h) = bounds.to_snapped_rect(1600, 841, bucket).expect("covers");
        assert_eq!((w, h), (64, 64));
        assert!(x <= 99 && y <= 99, "must not start after the content");
        assert!(
            x + w >= 151 && y + h >= 151,
            "must not end before the content"
        );
    }

    #[test]
    fn snapped_rect_shifts_back_at_the_far_edge() {
        // Content hard against the right edge: growing to a bucket would run
        // off, so the window slides left instead of clipping the content.
        let matrix = Matrix::create_box(
            15.0,
            40.0,
            Twips::from_pixels(1580.0),
            Twips::from_pixels(10.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        let (x, y, w, h) = bounds.to_snapped_rect(1600, 841, bucket).expect("covers");
        assert!(x + w <= 1600 && y + h <= 841, "stays inside the surface");
        assert!(x <= 1579 && x + w >= 1595, "still covers the content in x");
        assert!(y <= 9 && y + h >= 50, "still covers the content in y");
    }

    #[test]
    fn snapped_rect_never_exceeds_the_surface() {
        // Bucketing 1600 would round to 2048; the surface is the ceiling.
        let matrix = Matrix::create_box(
            4000.0,
            4000.0,
            Twips::from_pixels(-500.0),
            Twips::from_pixels(-500.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(
            bounds.to_snapped_rect(1600, 841, bucket),
            Some((0, 0, 1600, 841))
        );
    }

    #[test]
    fn snapped_rect_is_none_when_nothing_is_covered() {
        assert_eq!(
            ContentBounds::EMPTY.to_snapped_rect(1600, 841, bucket),
            None
        );
    }

    #[test]
    fn translated_moves_between_target_spaces() {
        let matrix = Matrix::create_box(
            10.0,
            10.0,
            Twips::from_pixels(100.0),
            Twips::from_pixels(200.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        let moved = bounds.translated(-100.0, -200.0);
        assert_eq!(moved.to_scissor(1600, 841, 0), Some((0, 0, 10, 10)));
        // Empty and unbounded are fixed points, so a shift cannot invent
        // coverage or lose it.
        assert!(ContentBounds::EMPTY.translated(5.0, 5.0).is_empty());
        assert_eq!(
            ContentBounds::UNBOUNDED.translated(5.0, 5.0),
            ContentBounds::UNBOUNDED
        );
    }

    #[test]
    fn area_is_the_clipped_rect() {
        let matrix = Matrix::create_box(
            100.0,
            50.0,
            Twips::from_pixels(0.0),
            Twips::from_pixels(0.0),
        );
        let mut bounds = ContentBounds::EMPTY;
        bounds.union_transformed(&matrix, ContentBounds::UNIT);
        assert_eq!(bounds.clipped_area(1600, 841), 100 * 50);
        assert_eq!(ContentBounds::EMPTY.clipped_area(1600, 841), 0);
    }

    #[test]
    fn from_points_bounds_a_vertex_run() {
        // How mesh bounds are taken: the extent of the tessellated vertices.
        let bounds = ContentBounds::from_points([(3.0, -2.0), (10.0, 4.0), (-1.0, 7.0)]);
        let mut placed = ContentBounds::EMPTY;
        placed.union_transformed(
            &Matrix::create_box(
                1.0,
                1.0,
                Twips::from_pixels(100.0),
                Twips::from_pixels(100.0),
            ),
            bounds,
        );
        // x: 99..110, y: 98..107 once shifted by the placement.
        assert_eq!(placed.to_scissor(1600, 841, 0), Some((99, 98, 11, 9)));
    }

    #[test]
    fn from_points_rejects_non_finite_vertices() {
        assert_eq!(
            ContentBounds::from_points([(0.0, 0.0), (f32::NAN, 1.0)]),
            ContentBounds::UNBOUNDED
        );
    }

    #[test]
    fn no_vertices_covers_nothing() {
        assert!(ContentBounds::from_points([]).is_empty());
    }
}
