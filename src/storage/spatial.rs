//! Spatial utility functions for distance calculations and filtering.

/// Calculate Euclidean distance between two tile coordinates.
pub fn tile_dist(proximity_x: u16, proximity_y: u16, grid_x: u16, grid_y: u16) -> f64 {
    let dx = (proximity_x as f64) - (grid_x as f64);
    let dy = (proximity_y as f64) - (grid_y as f64);
    ((dx * dx) + (dy * dy)).sqrt()
}

/// Returns the number of tiles per mile for a given zoom level
fn tiles_per_mile_by_zoom(zoom: u16) -> f64 {
    // Array of the pre-calculated ratio of number of tiles per mile at each zoom level
    //
    // 32 tiles is about 40 miles at z14, use this as our mile <=> tile conversion.
    // The formula is (32 / 40 ) / 1.5^(14-zoom).
    // Pow functions are not supported in constant functions in rust,
    // and a custom constant pow function can't be implemented because if statements and loops are not yet supported in constant functions.
    //
    // Note: the formula uses 1.5^(14-zoom) instead of 2^(14-zoom) to maintain consistency with
    // existing geocoding behavior. A truly consistent radius scaled across zoom levels would use 2 as the base,
    // but this would change proximity search behavior across different zoom levels.
    const TILES_PER_MILE_BY_ZOOM: [f64; 17] = [
        0.002740389912625401,
        0.004110584868938102,
        0.006165877303407152,
        0.009248815955110727,
        0.013873223932666092,
        0.020809835898999138,
        0.031214753848498707,
        0.046822130772748057,
        0.07023319615912209,
        0.10534979423868314,
        0.1580246913580247,
        0.23703703703703705,
        0.35555555555555557,
        0.5333333333333333,
        0.8,
        1.2000000000000002,
        1.8000000000000003,
    ];
    if zoom <= 16 {
        TILES_PER_MILE_BY_ZOOM[zoom as usize]
    } else {
        0.8 * 1.5_f64.powi((zoom - 14) as i32)
    }
}

/// Convert proximity radius from miles into scaled number of tiles
#[inline]
pub fn proximity_radius(zoom: u16, radius: f64) -> f64 {
    // Pre-calculated values exist for zooms 6-14, but the calculation is the same
    // for all zoom levels using tiles_per_mile_by_zoom
    radius * tiles_per_mile_by_zoom(zoom)
}

// We don't know the scale of the axis we're modeling, but it doesn't really
// matter as we just need internal consistency.
const E_POW: [f64; 8] = [
    1.,
    2.718281828459045,
    7.38905609893065,
    20.085536923187668,
    54.598150033144236,
    148.4131591025766,
    403.4287934927351,
    1096.6331584284585,
];

pub fn scoredist(mut zoom: u16, mut distance: f64, mut score: u8, radius: f64) -> f64 {
    if zoom < 6 {
        zoom = 6;
    }
    if score > 7 {
        score = 7;
    }

    // If the distance is 0, set a minimum distance to avoid dividing by distratios that approach zero
    if distance < 1. {
        distance = 0.8;
    }

    let mut dist_ratio: f64 = distance / proximity_radius(zoom, radius);

    // Beyond the proximity radius just let scoredist be driven by score.
    if dist_ratio > 1.0 {
        dist_ratio = 1.00;
    }
    ((6. * E_POW[score as usize] / E_POW[7]) + 1.) / dist_ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_dist_test() {
        assert_eq!(
            tile_dist(1, 1, 1, 1),
            0.,
            "Grid with the same x and y as as the proximity x and y should have tile_dist 0"
        );
        assert_eq!(
            tile_dist(1, 1, 1, 0),
            1.,
            "Grid one tile away from proximity tile should have tile_dist 1"
        );
        assert_eq!(
            tile_dist(1, 1, 0, 0),
            1.4142135623730951,
            "Grid diagonal from proximity tile should have tile_dist between 0 and 1 "
        );
    }

    #[test]
    fn tiles_per_mile_by_zoom_test() {
        assert_eq!(
            tiles_per_mile_by_zoom(14),
            0.8,
            "Tiles per mile for zoom 14 should be 0.8"
        );
        assert_eq!(
            tiles_per_mile_by_zoom(16),
            1.8000000000000003,
            "Tiles per mile should work for up to zoom 16"
        );
        assert_eq!(
            tiles_per_mile_by_zoom(6),
            0.031214753848498707,
            "Tiles per mile should work for down to zoom 6"
        );
    }

    #[test]
    fn proximity_radius_test() {
        assert_eq!(
            proximity_radius(14, 400.),
            320.,
            "Proximity radius in tiles for zoom 14, radius 400 is as expected"
        );
        assert_eq!(
            proximity_radius(16, 400.),
            720.0000000000001,
            "proximity_radius should work for zoom 14"
        );
        assert_eq!(
            proximity_radius(6, 0.),
            0.,
            "proximity_radius for a radius of 0 should be 0"
        );
        assert_eq!(
            proximity_radius(6, 40.),
            1.2485901539399482,
            "proximity_radius in tiles for zoom 6, radius 40 is as expected"
        );
        assert_eq!(
            proximity_radius(17, 400.),
            1080.0,
            "proximity_radius should work for zoom 17"
        );
    }
}
