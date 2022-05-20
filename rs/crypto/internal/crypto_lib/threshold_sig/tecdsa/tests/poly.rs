use ic_crypto_internal_threshold_sig_ecdsa::*;
use rand::Rng;

#[test]
fn poly_zero_times_zero_is_zero() -> ThresholdEcdsaResult<()> {
    for curve in EccCurveType::all() {
        let zero = EccScalar::zero(curve);

        for coeffs in 0..10 {
            let zpoly = Polynomial::new(curve, vec![zero; coeffs])?;
            assert!(zpoly.is_zero());
            let zpoly2 = zpoly.mul(&zpoly)?;
            assert!(zpoly2.is_zero());
        }
    }

    Ok(())
}

#[test]
fn poly_a_constant_poly_is_constant() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        let constant = EccScalar::random(curve, &mut rng)?;
        let poly = Polynomial::new(curve, vec![constant])?;

        for _trial in 0..100 {
            // For a constant polynomial, no matter where we evaluate
            // the result is the constant.
            let r = EccScalar::random(curve, &mut rng)?;
            assert_eq!(poly.evaluate_at(&r)?, constant);
        }
    }

    Ok(())
}

#[test]
fn poly_simple_polynomial_x_1() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    // Test the behavior of evaluating polynomial x+1

    for curve in EccCurveType::all() {
        let zero = EccScalar::zero(curve);
        let one = EccScalar::one(curve);
        let poly = Polynomial::new(curve, vec![one, one])?;

        assert_eq!(poly.evaluate_at(&zero)?, one);

        for _trial in 0..100 {
            let r = EccScalar::random(curve, &mut rng)?;
            let r_plus_1 = r.add(&one)?;
            assert_eq!(poly.evaluate_at(&r)?, r_plus_1);
        }
    }

    Ok(())
}

#[test]
fn poly_simple_polynomial_x2_x_1() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    // Test the behavior of evaluating polynomial x^2+x+1

    for curve in EccCurveType::all() {
        let zero = EccScalar::zero(curve);
        let one = EccScalar::one(curve);
        let poly = Polynomial::new(curve, vec![one, one, one])?;

        assert_eq!(poly.evaluate_at(&zero)?, one);

        for _trial in 0..100 {
            let r = EccScalar::random(curve, &mut rng)?;
            let r2 = r.mul(&r)?;
            let r2_plus_r_plus1 = r2.add(&r)?.add(&one)?;

            assert_eq!(poly.evaluate_at(&r)?, r2_plus_r_plus1);
        }
    }

    Ok(())
}

#[test]
fn poly_interpolate_works() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 0..50 {
            let poly = Polynomial::random(curve, num_coefficients, &mut rng)?;

            let mut samples = Vec::with_capacity(num_coefficients);
            for _i in 0..num_coefficients {
                let r = EccScalar::random(curve, &mut rng)?;
                let p_r = poly.evaluate_at(&r)?;
                samples.push((r, p_r));
            }
            assert_eq!(samples.len(), num_coefficients);

            let interp = Polynomial::interpolate(curve, &samples)?;
            assert_eq!(poly, interp);
        }
    }

    Ok(())
}

#[test]
fn poly_interpolate_fails_if_insufficient_points() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 1..50 {
            let poly = Polynomial::random(curve, num_coefficients, &mut rng)?;

            let mut samples = Vec::with_capacity(num_coefficients - 1);
            for _i in 0..num_coefficients - 1 {
                let r = EccScalar::random(curve, &mut rng)?;
                let p_r = poly.evaluate_at(&r)?;
                samples.push((r, p_r));
            }

            match Polynomial::interpolate(curve, &samples) {
                Err(e) => assert_eq!(e, ThresholdEcdsaError::InterpolationError),
                Ok(p) => assert_ne!(p, poly),
            }
        }
    }

    Ok(())
}

#[test]
fn poly_interpolate_errors_on_duplicate_inputs() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 0..50 {
            let poly = Polynomial::random(curve, num_coefficients, &mut rng)?;

            let mut samples = vec![];

            let dup_r = EccScalar::random(curve, &mut rng)?;
            let dup_p_r = poly.evaluate_at(&dup_r)?;

            for _i in 0..=num_coefficients {
                samples.push((dup_r, dup_p_r));
            }

            for _i in 0..=num_coefficients {
                let r = EccScalar::random(curve, &mut rng)?;
                let p_r = poly.evaluate_at(&r)?;
                samples.push((r, p_r));
                samples.push((dup_r, dup_p_r));
            }

            assert!(Polynomial::interpolate(curve, &samples).is_err());
        }
    }

    Ok(())
}

#[test]
fn poly_interpolate_is_resilient_to_low_x_points() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 0..50 {
            let poly = Polynomial::random(curve, num_coefficients, &mut rng)?;
            assert_eq!(poly.non_zero_coefficients(), num_coefficients);

            let one = EccScalar::one(curve);
            let mut x = EccScalar::zero(curve);

            let mut samples = vec![];

            for _i in 0..=num_coefficients {
                let p_x = poly.evaluate_at(&x)?;
                samples.push((x, p_x));
                x = x.add(&one)?;
            }

            let interp = Polynomial::interpolate(curve, &samples)?;
            assert_eq!(poly, interp);
        }
    }

    Ok(())
}

#[test]
fn poly_threshold_secret_sharing() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        let zero = EccScalar::zero(curve);
        let secret = EccScalar::random(curve, &mut rng)?;

        for num_coefficients in 1..50 {
            let poly = Polynomial::random_with_constant(secret, num_coefficients, &mut rng)?;
            assert_eq!(poly.non_zero_coefficients(), num_coefficients);

            let mut shares = Vec::with_capacity(num_coefficients + 1);
            for _i in 0..num_coefficients + 1 {
                let r = EccScalar::random(curve, &mut rng)?;
                let p_r = poly.evaluate_at(&r)?;
                shares.push((r, p_r));
            }

            let interp = Polynomial::interpolate(curve, &shares)?;
            assert_eq!(interp.evaluate_at(&zero)?, secret);
        }
    }

    Ok(())
}

#[test]
fn poly_simple_commitments() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 1..50 {
            let poly = Polynomial::random(curve, num_coefficients, &mut rng)?;
            let _commitment = SimpleCommitment::create(&poly, num_coefficients)?;
        }
    }

    Ok(())
}

#[test]
fn poly_pedersen_commitments() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 1..50 {
            let poly_a = Polynomial::random(curve, num_coefficients, &mut rng)?;
            let poly_b = Polynomial::random(curve, num_coefficients, &mut rng)?;
            let commitment_ab = PedersenCommitment::create(&poly_a, &poly_b, num_coefficients)?;
            let commitment_ba = PedersenCommitment::create(&poly_b, &poly_a, num_coefficients)?;

            assert_ne!(commitment_ab, commitment_ba);
        }
    }

    Ok(())
}

#[test]
#[allow(clippy::identity_op)]
fn poly_lagrange_coefficients_at_zero_are_correct() {
    let curve = EccCurveType::K256;

    fn int_to_scalars(curve: EccCurveType, ints: &[i64]) -> Vec<EccScalar> {
        let mut scalars = Vec::with_capacity(ints.len());
        for i in ints {
            let s = EccScalar::from_u64(curve, i.abs() as u64);
            scalars.push(if i.is_negative() { s.negate() } else { s });
        }
        scalars
    }

    let x_values = [1, 2, 3, 6];

    let lagrange_numerators = int_to_scalars(curve, &[3 * 4 * 7, 2 * 4 * 7, 2 * 3 * 7, 2 * 3 * 4]);

    let lagrange_denominators = int_to_scalars(
        curve,
        &[
            (3 - 2) * (4 - 2) * (7 - 2),
            (2 - 3) * (4 - 3) * (7 - 3),
            (2 - 4) * (3 - 4) * (7 - 4),
            (2 - 7) * (3 - 7) * (4 - 7),
        ],
    );

    let computed = lagrange_numerators
        .iter()
        .zip(lagrange_denominators)
        .map(|(numerator, denominator)| numerator.mul(&denominator.invert().unwrap()).unwrap())
        .collect::<Vec<EccScalar>>();

    let zero = EccScalar::zero(curve);

    let observed = LagrangeCoefficients::at_value(&zero, &x_values)
        .expect("Failed even though coordinates were distinct")
        .coefficients()
        .to_vec();

    assert_eq!(computed, observed);
}

fn interpolation_at_zero(x: &[NodeIndex], y: &[EccPoint]) -> ThresholdEcdsaResult<EccPoint> {
    let curve_type = y[0].curve_type();
    let coefficients = LagrangeCoefficients::at_zero(curve_type, x)?;
    coefficients.interpolate_point(y)
}

fn random_node_indexes(count: usize) -> Vec<NodeIndex> {
    let mut rng = rand::thread_rng();

    let mut set = std::collections::BTreeSet::new();

    while set.len() != count {
        let r = rng.gen::<NodeIndex>();
        set.insert(r);
    }

    set.iter().cloned().collect()
}

#[test]
fn poly_point_interpolation_at_zero() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 1..30 {
            let sk = EccScalar::random(curve, &mut rng)?;
            let pk = EccPoint::mul_by_g(&sk)?;

            let poly = Polynomial::random_with_constant(sk, num_coefficients, &mut rng)?;

            let x = random_node_indexes(num_coefficients);
            let mut y = Vec::with_capacity(num_coefficients);

            for r in &x {
                let p_r = poly.evaluate_at(&EccScalar::from_node_index(curve, *r))?;
                let g_p_r = EccPoint::mul_by_g(&p_r)?;
                y.push(g_p_r);
            }

            let g0 = interpolation_at_zero(&x, &y)?;
            assert_eq!(g0, pk);
        }
    }

    Ok(())
}

#[test]
fn poly_point_interpolation_at_value() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 1..30 {
            let value = EccScalar::random(curve, &mut rng)?;

            let poly = Polynomial::random(curve, num_coefficients, &mut rng)?;

            let x = random_node_indexes(num_coefficients);
            let mut y = Vec::with_capacity(num_coefficients);

            for r in &x {
                let p_r = poly.evaluate_at(&EccScalar::from_node_index(curve, *r))?;
                let g_p_r = EccPoint::mul_by_g(&p_r)?;
                y.push(g_p_r);
            }

            let coefficients = LagrangeCoefficients::at_value(&value, &x)?;

            assert_eq!(
                coefficients.interpolate_point(&y)?,
                EccPoint::mul_by_g(&poly.evaluate_at(&value)?)?
            );
        }
    }

    Ok(())
}

#[test]
fn poly_scalar_interpolation_at_value() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 1..30 {
            let value = EccScalar::random(curve, &mut rng)?;

            let poly = Polynomial::random(curve, num_coefficients, &mut rng)?;

            let x = random_node_indexes(num_coefficients);
            let mut y = Vec::with_capacity(num_coefficients);

            for r in &x {
                let p_r = poly.evaluate_at(&EccScalar::from_node_index(curve, *r))?;
                y.push(p_r);
            }

            let coefficients = LagrangeCoefficients::at_value(&value, &x)?;

            assert_eq!(
                coefficients.interpolate_scalar(&y)?,
                poly.evaluate_at(&value)?
            );
        }
    }

    Ok(())
}

#[test]
fn poly_point_interpolation_at_zero_rejects_duplicates() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 1..10 {
            let sk = EccScalar::random(curve, &mut rng)?;
            let poly = Polynomial::random_with_constant(sk, num_coefficients, &mut rng)?;

            let mut x = random_node_indexes(num_coefficients);
            let mut y = Vec::with_capacity(num_coefficients + 1);

            x.push(x[rng.gen::<usize>() % x.len()]);

            for r in &x {
                let g_p_r =
                    EccPoint::mul_by_g(&poly.evaluate_at(&EccScalar::from_node_index(curve, *r))?)?;
                y.push(g_p_r);
            }

            assert!(interpolation_at_zero(&x, &y).is_err());
        }
    }

    Ok(())
}

#[test]
fn poly_point_interpolation_at_zero_fails_with_insufficient_shares() -> ThresholdEcdsaResult<()> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        for num_coefficients in 2..20 {
            let sk = EccScalar::random(curve, &mut rng)?;
            let pk = EccPoint::mul_by_g(&sk)?;

            let poly = Polynomial::random_with_constant(sk, num_coefficients, &mut rng)?;
            let x = random_node_indexes(num_coefficients - 1);
            let mut y = Vec::with_capacity(num_coefficients - 1);

            for r in &x {
                let g_p_r =
                    EccPoint::mul_by_g(&poly.evaluate_at(&EccScalar::from_node_index(curve, *r))?)?;
                y.push(g_p_r);
            }

            // Interpolation fails to recover the correct value with insufficient values
            match interpolation_at_zero(&x, &y) {
                Err(e) => assert_eq!(e, ThresholdEcdsaError::InterpolationError),
                Ok(pt) => assert_ne!(pt, pk),
            }
        }
    }

    Ok(())
}

#[test]
fn polynomial_should_redact_logs() -> Result<(), ThresholdEcdsaError> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        let constant = EccScalar::random(curve, &mut rng)?;
        let poly = Polynomial::new(curve, vec![constant])?;
        let log = format!("{:?}", poly);
        assert_eq!(
            format!("Polynomial {{curve: {:?}, coefficients: REDACTED}}", curve),
            log
        );
    }

    Ok(())
}

#[test]
fn commitment_opening_should_redact_logs() -> Result<(), ThresholdEcdsaError> {
    let mut rng = rand::thread_rng();

    for curve in EccCurveType::all() {
        let scalar = EccScalar::random(curve, &mut rng)?;
        let opening = CommitmentOpening::Simple(scalar);
        let log = format!("{:?}", opening);
        assert_eq!(
            format!("CommitmentOpening::Simple({:?}(REDACTED))", curve),
            log
        );
    }

    Ok(())
}

#[test]
fn simple_commitment_to_bytes_is_stable() {
    fn simple_commitment_bytes(curve: EccCurveType, sz: usize) -> Vec<u8> {
        let mut rng = Seed::from_bytes(&vec![42; sz]).into_rng();

        let polynomial =
            Polynomial::random(curve, sz, &mut rng).expect("Polynomial::random failed");

        let opening =
            SimpleCommitment::create(&polynomial, sz).expect("SimpleCommitment::create failed");

        let opening = PolynomialCommitment::Simple(opening);
        opening.to_bytes()
    }

    assert_eq!(
        hex::encode(simple_commitment_bytes(EccCurveType::K256, 1)),
        "5301039fe83fd99e3c6b76df143254cb8a836d6b47aa68cf3fd7581196afdeb904f9d2"
    );
    assert_eq!(
        hex::encode(simple_commitment_bytes(EccCurveType::K256, 2)),
        "530103c1f401c44781086b6d0e6b0ef5c4b44615704d089d454570a15f5d57ec5c55820347ab40cedd0e1d19945028dd62a945d7f48083cef4174753c27925b26912af3f"
    );

    assert_eq!(
        hex::encode(simple_commitment_bytes(EccCurveType::P256, 1)),
        "5302032aaa236410513db103d17be5d6f52f1444bdd4e440a20e65c481b56d86a431bc"
    );
    assert_eq!(
        hex::encode(simple_commitment_bytes(EccCurveType::P256, 2)),
        "530203ebb013814bb1b37b43cf69423883313209fa39a52dae48babe7295010002991c03bfd2404c3905d7fa0a9ab22549ab5c5a34e275d4fbf6953ca2afdd92289b4ae7"
    );
}

#[test]
fn pedersen_commitment_to_bytes_is_stable() {
    fn pedersen_commitment_bytes(curve: EccCurveType, sz: usize) -> Vec<u8> {
        let mut rng = Seed::from_bytes(&vec![42; sz]).into_rng();

        let polynomial =
            Polynomial::random(curve, sz, &mut rng).expect("Polynomial::random failed");
        let mask = Polynomial::random(curve, sz, &mut rng).expect("Polynomial::random failed");

        let opening = PedersenCommitment::create(&polynomial, &mask, sz)
            .expect("PedersenCommitment::create failed");

        let opening = PolynomialCommitment::Pedersen(opening);
        opening.to_bytes()
    }

    assert_eq!(
        hex::encode(pedersen_commitment_bytes(EccCurveType::K256, 1)),
        "5001025b20c6c5dec68e994ec79c9dbdc4657eedda2bce11d3cfd4e9d328b56e25b288"
    );
    assert_eq!(
        hex::encode(pedersen_commitment_bytes(EccCurveType::K256, 2)),
        "5001032d67617d5848b9401d292fd026b062bdf1c48e8471608b1983ee54b773e61cdb03b4ee72750211155139f613cb32897854152d4f6cb41c843963075c7146dc6001"
    );

    assert_eq!(
        hex::encode(pedersen_commitment_bytes(EccCurveType::P256, 1)),
        "5002026662cd78cf51a8afee0a158b49c9d222930168a96488fe7d574e42585126b085"
    );
    assert_eq!(
        hex::encode(pedersen_commitment_bytes(EccCurveType::P256, 2)),
        "5002027facbc97df263004792094aa69d0e00e14a9f490f052c20b49abd6e981da2017023720c8f9a0d53edf8e285a47737b47bae702309546a51adf1b8fc2c13db3417f"
    );
}
