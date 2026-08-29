import 'package:flutter/material.dart';

abstract final class FusionColors {
  static const primary = Color(0xFF171717);
  static const onPrimary = Colors.white;
  static const primaryContainer = Color(0xFFE8E8E8);
  static const onPrimaryContainer = Color(0xFF171717);
  static const inversePrimary = Color(0xFFDADADA);
  static const secondary = Color(0xFF505050);
  static const onSecondary = Colors.white;
  static const secondaryContainer = Color(0xFFE5E5E5);
  static const onSecondaryContainer = Color(0xFF1C1C1C);
  static const tertiary = Color(0xFF686868);
  static const onTertiary = Colors.white;
  static const tertiaryContainer = Color(0xFFE2E2E2);
  static const onTertiaryContainer = Color(0xFF202020);
  static const background = Color(0xFFF7F7F7);
  static const onBackground = Color(0xFF171717);
  static const surface = Colors.white;
  static const onSurface = Color(0xFF1A1A1A);
  static const surfaceVariant = Color(0xFFE9E9E9);
  static const onSurfaceVariant = Color(0xFF5F5F5F);
  static const outline = Color(0xFF8A8A8A);
  static const outlineVariant = Color(0xFFD4D4D4);
  static const inverseSurface = Color(0xFF242424);
  static const inverseOnSurface = Color(0xFFF5F5F5);
  static const error = Color(0xFF303030);
  static const onError = Colors.white;
  static const errorContainer = Color(0xFFE3E3E3);
  static const onErrorContainer = Color(0xFF202020);
  static const scrim = Colors.black;
  static const surfaceBright = Colors.white;
  static const surfaceDim = Color(0xFFE1E1E1);
  static const surfaceContainer = Color(0xFFF5F5F5);
  static const surfaceContainerHigh = Color(0xFFEEEEEE);
  static const surfaceContainerHighest = Color(0xFFE7E7E7);
  static const surfaceContainerLow = Color(0xFFFAFAFA);
  static const surfaceContainerLowest = Colors.white;
  static const primaryFixed = Color(0xFFE8E8E8);
  static const primaryFixedDim = Color(0xFFCBCBCB);
  static const onPrimaryFixed = Color(0xFF171717);
  static const onPrimaryFixedVariant = Color(0xFF444444);
  static const secondaryFixed = Color(0xFFE5E5E5);
  static const secondaryFixedDim = Color(0xFFC8C8C8);
  static const onSecondaryFixed = Color(0xFF1C1C1C);
  static const onSecondaryFixedVariant = Color(0xFF494949);
  static const tertiaryFixed = Color(0xFFE2E2E2);
  static const tertiaryFixedDim = Color(0xFFC4C4C4);
  static const onTertiaryFixed = Color(0xFF202020);
  static const onTertiaryFixedVariant = Color(0xFF505050);
  static const playerFallback = Color(0xFFEAEAEA);
}

abstract final class FusionTypography {
  // Android's generic sans-serif family resolves through the system font.
  // Naming it explicitly avoids Flutter's bundled
  // default font taking precedence for Latin text and numerals.
  static const String fontFamily = 'sans-serif';
  static const regularFontWeight = FontWeight.w400;
  static const mediumFontWeight = FontWeight.w500;
  static const semiBoldFontWeight = FontWeight.w600;
  static const boldFontWeight = FontWeight.w700;

  // Match the Android typography contract directly: body copy is 400,
  // interactive labels are 500, regular headings are 600, and the highest
  // emphasis level is 700. Keep FontWeight and the variable-font axis aligned
  // so the platform sans font does not render lighter than its semantic role.
  static const regular = <FontVariation>[FontVariation('wght', 400)];
  static const medium = <FontVariation>[FontVariation('wght', 500)];
  static const semiBold = <FontVariation>[FontVariation('wght', 600)];
  static const bold = <FontVariation>[FontVariation('wght', 700)];

  static const regularStyle = TextStyle(
    fontFamily: fontFamily,
    fontWeight: regularFontWeight,
    fontVariations: regular,
  );
  static const mediumStyle = TextStyle(
    fontFamily: fontFamily,
    fontWeight: mediumFontWeight,
    fontVariations: medium,
  );
  static const semiBoldStyle = TextStyle(
    fontFamily: fontFamily,
    fontWeight: semiBoldFontWeight,
    fontVariations: semiBold,
  );
  static const boldStyle = TextStyle(
    fontFamily: fontFamily,
    fontWeight: boldFontWeight,
    fontVariations: bold,
  );
}

/// Applies the Android FusionPlay body profile to bare text while the
/// Material theme supplies the matching semantic weight for each text role.
class FusionTypographyScope extends StatelessWidget {
  const FusionTypographyScope({super.key, required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) => DefaultTextStyle.merge(
    style: FusionTypography.regularStyle,
    child: child,
  );
}

abstract final class SettingsStyle {
  static const panelWidth = 400.0;
  static const blurRadius = 20.0;
  static const dimAlpha = 0.08;
  static const blurEnter = Duration(milliseconds: 220);
  static const blurExit = Duration(milliseconds: 160);
  static const panelEnter = Duration(milliseconds: 260);
  static const panelExit = Duration(milliseconds: 180);
  static const panelFadeIn = Duration(milliseconds: 160);
  static const panelFadeOut = Duration(milliseconds: 120);
  static const panelBackground = Colors.white;
  static const groupBackground = Color(0xFFEAEAEA);
  static const primaryText = Color(0xFF151515);
  static const secondaryText = Color(0xFF555555);
  static const iconColor = Color(0xFF1F1F1F);
  static const inputBackground = Colors.white;
  static const inputText = Color(0xFF202020);
  static const switchOnTrack = Color(0xFF68CE67);
  static const switchOnThumb = Colors.white;
  static const switchOffTrack = Colors.white;
  static const switchOffThumb = Color(0xFF242424);
  static const inactiveAction = Color(0xFFD1D1D1);
  static const cardRadius = 28.0;
  static const fieldRadius = 16.0;
  static const cardSpacing = 16.0;
  static const clusteredSpacing = 3.0;
  static const headerBottomInset = 4.0;
  static const fieldHorizontalPadding = 20.0;
  static const fieldVerticalPadding = 16.0;
  static const joinedFieldPadding = 6.0;
  static const fieldTitleVisualTopInset = 25.0;
  static const fieldTitleGap = 12.0;
  static const rowContentGap = 14.0;
  static const leadingSize = 42.0;
  static const leadingIconSize = 24.0;
  static const fieldHorizontalContentPadding = 16.0;
  static const nameActionGap = 12.0;
  static const panelHorizontalPadding = 28.0;
  static const panelBottomPadding = 30.0;
  static const panelTopInset = 74.0;
  static const titleHeight = 56.0;
  static const compactActionWidth = 82.0;
  static const compactActionHeight = 42.0;
  static const nameActionHeight = 50.0;
  static const fieldHeight = 56.0;
  static const switchWidth = 52.0;
  static const switchHeight = 32.0;
  static const actionColorDuration = Duration(milliseconds: 160);
}

abstract final class FusionMotion {
  static const easeOut = Cubic(.23, 1, .32, 1);
  static const buttonPress = Duration(milliseconds: 120);
  static const buttonRelease = Duration(milliseconds: 160);
  static const stateChange = Duration(milliseconds: 160);
}

ThemeData fusionTheme() {
  const scheme = ColorScheme.light(
    primary: FusionColors.primary,
    onPrimary: FusionColors.onPrimary,
    primaryContainer: FusionColors.primaryContainer,
    onPrimaryContainer: FusionColors.onPrimaryContainer,
    inversePrimary: FusionColors.inversePrimary,
    primaryFixed: FusionColors.primaryFixed,
    primaryFixedDim: FusionColors.primaryFixedDim,
    onPrimaryFixed: FusionColors.onPrimaryFixed,
    onPrimaryFixedVariant: FusionColors.onPrimaryFixedVariant,
    secondary: FusionColors.secondary,
    onSecondary: FusionColors.onSecondary,
    secondaryContainer: FusionColors.secondaryContainer,
    onSecondaryContainer: FusionColors.onSecondaryContainer,
    secondaryFixed: FusionColors.secondaryFixed,
    secondaryFixedDim: FusionColors.secondaryFixedDim,
    onSecondaryFixed: FusionColors.onSecondaryFixed,
    onSecondaryFixedVariant: FusionColors.onSecondaryFixedVariant,
    tertiary: FusionColors.tertiary,
    onTertiary: FusionColors.onTertiary,
    tertiaryContainer: FusionColors.tertiaryContainer,
    onTertiaryContainer: FusionColors.onTertiaryContainer,
    tertiaryFixed: FusionColors.tertiaryFixed,
    tertiaryFixedDim: FusionColors.tertiaryFixedDim,
    onTertiaryFixed: FusionColors.onTertiaryFixed,
    onTertiaryFixedVariant: FusionColors.onTertiaryFixedVariant,
    error: FusionColors.error,
    onError: FusionColors.onError,
    errorContainer: FusionColors.errorContainer,
    onErrorContainer: FusionColors.onErrorContainer,
    surface: FusionColors.surface,
    onSurface: FusionColors.onSurface,
    surfaceDim: FusionColors.surfaceDim,
    surfaceBright: FusionColors.surfaceBright,
    surfaceContainerLowest: FusionColors.surfaceContainerLowest,
    surfaceContainerLow: FusionColors.surfaceContainerLow,
    surfaceContainer: FusionColors.surfaceContainer,
    surfaceContainerHigh: FusionColors.surfaceContainerHigh,
    surfaceContainerHighest: FusionColors.surfaceContainerHighest,
    onSurfaceVariant: FusionColors.onSurfaceVariant,
    outline: FusionColors.outline,
    outlineVariant: FusionColors.outlineVariant,
    scrim: FusionColors.scrim,
    inverseSurface: FusionColors.inverseSurface,
    onInverseSurface: FusionColors.inverseOnSurface,
    surfaceTint: FusionColors.primary,
  );
  const typography = TextTheme(
    displayLarge: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 57,
      height: 64 / 57,
      fontWeight: FusionTypography.regularFontWeight,
      fontVariations: FusionTypography.regular,
    ),
    displayMedium: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 45,
      height: 52 / 45,
      fontWeight: FusionTypography.regularFontWeight,
      fontVariations: FusionTypography.regular,
    ),
    displaySmall: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 36,
      height: 44 / 36,
      fontWeight: FusionTypography.semiBoldFontWeight,
      fontVariations: FusionTypography.semiBold,
    ),
    headlineLarge: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 32,
      height: 40 / 32,
      fontWeight: FusionTypography.regularFontWeight,
      fontVariations: FusionTypography.regular,
    ),
    headlineMedium: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 28,
      height: 36 / 28,
      fontWeight: FusionTypography.semiBoldFontWeight,
      fontVariations: FusionTypography.semiBold,
    ),
    headlineSmall: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 24,
      height: 32 / 24,
      fontWeight: FusionTypography.regularFontWeight,
      fontVariations: FusionTypography.regular,
    ),
    titleLarge: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 22,
      height: 28 / 22,
      fontWeight: FusionTypography.semiBoldFontWeight,
      fontVariations: FusionTypography.semiBold,
    ),
    titleMedium: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 16,
      height: 1.5,
      fontWeight: FusionTypography.semiBoldFontWeight,
      fontVariations: FusionTypography.semiBold,
    ),
    titleSmall: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 14,
      height: 20 / 14,
      fontWeight: FusionTypography.mediumFontWeight,
      fontVariations: FusionTypography.medium,
    ),
    bodyLarge: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 16,
      height: 1.5,
      fontWeight: FusionTypography.regularFontWeight,
      fontVariations: FusionTypography.regular,
    ),
    bodyMedium: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 14,
      height: 20 / 14,
      fontWeight: FusionTypography.regularFontWeight,
      fontVariations: FusionTypography.regular,
    ),
    bodySmall: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 12,
      height: 16 / 12,
      fontWeight: FusionTypography.regularFontWeight,
      fontVariations: FusionTypography.regular,
    ),
    labelLarge: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 14,
      height: 20 / 14,
      fontWeight: FusionTypography.mediumFontWeight,
      fontVariations: FusionTypography.medium,
    ),
    labelMedium: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 12,
      height: 16 / 12,
      fontWeight: FusionTypography.mediumFontWeight,
      fontVariations: FusionTypography.medium,
    ),
    labelSmall: TextStyle(
      fontFamily: FusionTypography.fontFamily,
      fontSize: 11,
      height: 16 / 11,
      fontWeight: FusionTypography.mediumFontWeight,
      fontVariations: FusionTypography.medium,
    ),
  );
  return ThemeData(
    useMaterial3: true,
    colorScheme: scheme,
    fontFamily: FusionTypography.fontFamily,
    scaffoldBackgroundColor: FusionColors.background,
    textTheme: typography.apply(
      bodyColor: FusionColors.onSurface,
      displayColor: FusionColors.onSurface,
    ),
    primaryTextTheme: typography.apply(
      bodyColor: FusionColors.onPrimary,
      displayColor: FusionColors.onPrimary,
    ),
    popupMenuTheme: PopupMenuThemeData(
      textStyle: typography.bodyMedium,
      labelTextStyle: WidgetStatePropertyAll(typography.bodyMedium),
    ),
    tooltipTheme: TooltipThemeData(textStyle: typography.bodySmall),
    textButtonTheme: TextButtonThemeData(
      style: ButtonStyle(
        textStyle: WidgetStatePropertyAll(typography.labelLarge),
      ),
    ),
    filledButtonTheme: FilledButtonThemeData(
      style: ButtonStyle(
        textStyle: WidgetStatePropertyAll(typography.labelLarge),
      ),
    ),
    elevatedButtonTheme: ElevatedButtonThemeData(
      style: ButtonStyle(
        textStyle: WidgetStatePropertyAll(typography.labelLarge),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: ButtonStyle(
        textStyle: WidgetStatePropertyAll(typography.labelLarge),
      ),
    ),
    splashFactory: NoSplash.splashFactory,
    highlightColor: Colors.transparent,
    hoverColor: Colors.transparent,
    focusColor: Colors.transparent,
  );
}
