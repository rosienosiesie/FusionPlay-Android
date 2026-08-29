param(
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$sourcePath = Join-Path $ProjectRoot "brand-FusionPlay-Mark.png"
$resourcePath = Join-Path $ProjectRoot "flutter\android\app\src\main\res"

if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
    throw "FusionPlay icon source was not found: $sourcePath"
}

$generatorSource = @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.IO;

public static class FusionPlayAndroidIconGenerator
{
    private const int AdaptiveCanvas = 432;
    // Keep the FP mark inside a conservative adaptive-icon keyline. The
    // previous 252 px mark occupied 58.3% of the canvas and looked oversized
    // after HyperOS applied its rounded-square mask. 190 px is 44.0%, which
    // matches the visual weight of neighboring Material-style launcher icons.
    private const int AdaptiveLogoWidth = 190;

    public static void Generate(string sourcePath, string resourcePath)
    {
        using (var source = new Bitmap(sourcePath))
        using (var logoMask = ExtractLogo(source))
        using (var background = CreateBrandGradient(AdaptiveCanvas))
        using (var foreground = CreateAdaptiveForeground(logoMask))
        {
            var drawableNoDpi = Path.Combine(resourcePath, "drawable-nodpi");
            Directory.CreateDirectory(drawableNoDpi);
            background.Save(
                Path.Combine(drawableNoDpi, "ic_launcher_background_art_v3.png"),
                ImageFormat.Png
            );
            foreground.Save(
                Path.Combine(drawableNoDpi, "ic_launcher_foreground_mark_v3.png"),
                ImageFormat.Png
            );
            using (var legacy = ComposeLegacy(background, foreground, false))
            using (var round = ComposeLegacy(background, foreground, true))
            {
                WriteDensity(resourcePath, "mdpi", 48, legacy, round);
                WriteDensity(resourcePath, "hdpi", 72, legacy, round);
                WriteDensity(resourcePath, "xhdpi", 96, legacy, round);
                WriteDensity(resourcePath, "xxhdpi", 144, legacy, round);
                WriteDensity(resourcePath, "xxxhdpi", 192, legacy, round);
            }
        }
    }

    private static Bitmap ExtractLogo(Bitmap source)
    {
        int width = source.Width;
        int height = source.Height;
        var core = new bool[width * height];

        int left = (int)(width * 0.12f);
        int right = (int)(width * 0.88f);
        int top = (int)(height * 0.19f);
        int bottom = (int)(height * 0.84f);
        for (int y = top; y < bottom; y++)
        {
            for (int x = left; x < right; x++)
            {
                Color color = source.GetPixel(x, y);
                int min = Math.Min(color.R, Math.Min(color.G, color.B));
                int max = Math.Max(color.R, Math.Max(color.G, color.B));
                int average = (color.R + color.G + color.B) / 3;
                core[y * width + x] =
                    color.A > 220 && min >= 232 && max - min <= 38 && average >= 238;
            }
        }

        var selected = SelectLogoComponents(core, width, height);
        var output = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        for (int y = top; y < bottom; y++)
        {
            for (int x = left; x < right; x++)
            {
                int index = y * width + x;
                int alpha = selected[index] ? 255 : EdgeAlpha(source, selected, width, height, x, y);
                if (alpha > 0)
                {
                    output.SetPixel(x, y, Color.FromArgb(alpha, 255, 255, 255));
                }
            }
        }
        return CropToVisibleBounds(output);
    }

    private static bool[] SelectLogoComponents(bool[] candidates, int width, int height)
    {
        var visited = new bool[candidates.Length];
        var selected = new bool[candidates.Length];
        var queue = new Queue<int>();
        int[] offsets = { -1, 1, -width, width };

        for (int start = 0; start < candidates.Length; start++)
        {
            if (!candidates[start] || visited[start]) continue;
            var component = new List<int>();
            int minX = width;
            int minY = height;
            int maxX = -1;
            int maxY = -1;
            visited[start] = true;
            queue.Enqueue(start);

            while (queue.Count > 0)
            {
                int index = queue.Dequeue();
                component.Add(index);
                int x = index % width;
                int y = index / width;
                minX = Math.Min(minX, x);
                maxX = Math.Max(maxX, x);
                minY = Math.Min(minY, y);
                maxY = Math.Max(maxY, y);

                foreach (int offset in offsets)
                {
                    int next = index + offset;
                    if (next < 0 || next >= candidates.Length || visited[next] || !candidates[next])
                    {
                        continue;
                    }
                    int nextX = next % width;
                    int nextY = next / width;
                    if (Math.Abs(nextX - x) + Math.Abs(nextY - y) != 1) continue;
                    visited[next] = true;
                    queue.Enqueue(next);
                }
            }

            int componentWidth = maxX - minX + 1;
            int componentHeight = maxY - minY + 1;
            if (component.Count >= 2400 && componentWidth >= 90 && componentHeight >= 90)
            {
                foreach (int index in component) selected[index] = true;
            }
        }
        return selected;
    }

    private static int EdgeAlpha(
        Bitmap source,
        bool[] selected,
        int width,
        int height,
        int x,
        int y
    )
    {
        Color color = source.GetPixel(x, y);
        int min = Math.Min(color.R, Math.Min(color.G, color.B));
        int max = Math.Max(color.R, Math.Max(color.G, color.B));
        int average = (color.R + color.G + color.B) / 3;
        if (color.A < 96 || min < 185 || max - min > 105 || average < 208) return 0;

        const int radius = 4;
        for (int dy = -radius; dy <= radius; dy++)
        {
            int sampleY = y + dy;
            if (sampleY < 0 || sampleY >= height) continue;
            for (int dx = -radius; dx <= radius; dx++)
            {
                int sampleX = x + dx;
                if (sampleX < 0 || sampleX >= width) continue;
                if (selected[sampleY * width + sampleX])
                {
                    double white = Math.Max(0.0, Math.Min(1.0, (min - 180.0) / 65.0));
                    double neutral = Math.Max(0.0, Math.Min(1.0, (115.0 - (max - min)) / 85.0));
                    return (int)Math.Round(255.0 * white * neutral);
                }
            }
        }
        return 0;
    }

    private static Bitmap CropToVisibleBounds(Bitmap source)
    {
        int minX = source.Width;
        int minY = source.Height;
        int maxX = -1;
        int maxY = -1;
        for (int y = 0; y < source.Height; y++)
        {
            for (int x = 0; x < source.Width; x++)
            {
                if (source.GetPixel(x, y).A == 0) continue;
                minX = Math.Min(minX, x);
                maxX = Math.Max(maxX, x);
                minY = Math.Min(minY, y);
                maxY = Math.Max(maxY, y);
            }
        }
        if (maxX < minX || maxY < minY) throw new InvalidOperationException("Unable to isolate the FusionPlay mark.");

        var crop = new Bitmap(maxX - minX + 1, maxY - minY + 1, PixelFormat.Format32bppArgb);
        using (Graphics graphics = Graphics.FromImage(crop))
        {
            graphics.CompositingMode = CompositingMode.SourceCopy;
            graphics.DrawImage(
                source,
                new Rectangle(0, 0, crop.Width, crop.Height),
                new Rectangle(minX, minY, crop.Width, crop.Height),
                GraphicsUnit.Pixel
            );
        }
        return crop;
    }

    private static Bitmap CreateAdaptiveForeground(Bitmap logo)
    {
        int targetWidth = AdaptiveLogoWidth;
        int targetHeight = Math.Max(1, (int)Math.Round(logo.Height * (targetWidth / (double)logo.Width)));
        var canvas = new Bitmap(AdaptiveCanvas, AdaptiveCanvas, PixelFormat.Format32bppArgb);
        using (Graphics graphics = Graphics.FromImage(canvas))
        {
            ConfigureHighQuality(graphics);
            graphics.CompositingMode = CompositingMode.SourceCopy;
            graphics.DrawImage(
                logo,
                new Rectangle(
                    (AdaptiveCanvas - targetWidth) / 2,
                    (AdaptiveCanvas - targetHeight) / 2,
                    targetWidth,
                    targetHeight
                )
            );
        }
        return canvas;
    }

    private static Bitmap CreateBrandGradient(int size)
    {
        var bitmap = new Bitmap(size, size, PixelFormat.Format32bppArgb);
        var top = new[]
        {
            Color.FromArgb(255, 251, 65, 151),
            Color.FromArgb(255, 255, 98, 55),
            Color.FromArgb(255, 255, 216, 63),
            Color.FromArgb(255, 86, 221, 157),
        };
        var middle = new[]
        {
            Color.FromArgb(255, 239, 67, 173),
            Color.FromArgb(255, 245, 92, 111),
            Color.FromArgb(255, 59, 204, 188),
            Color.FromArgb(255, 31, 184, 224),
        };
        var bottom = new[]
        {
            Color.FromArgb(255, 169, 48, 226),
            Color.FromArgb(255, 118, 56, 224),
            Color.FromArgb(255, 71, 77, 222),
            Color.FromArgb(255, 35, 91, 235),
        };
        var random = new Random(4650);
        for (int y = 0; y < size; y++)
        {
            double ny = y / (double)(size - 1);
            for (int x = 0; x < size; x++)
            {
                double nx = x / (double)(size - 1);
                Color upper = Mix(SampleRow(top, nx), SampleRow(middle, nx), SmoothStep(Math.Min(1.0, ny * 2.0)));
                Color lower = Mix(SampleRow(middle, nx), SampleRow(bottom, nx), SmoothStep(Math.Max(0.0, ny * 2.0 - 1.0)));
                Color color = ny < 0.5 ? upper : lower;
                int dither = random.Next(-1, 2);
                bitmap.SetPixel(
                    x,
                    y,
                    Color.FromArgb(
                        255,
                        Clamp(color.R + dither),
                        Clamp(color.G + dither),
                        Clamp(color.B + dither)
                    )
                );
            }
        }
        return bitmap;
    }

    private static Color SampleRow(Color[] colors, double position)
    {
        double scaled = Math.Max(0.0, Math.Min(1.0, position)) * (colors.Length - 1);
        int index = Math.Min(colors.Length - 2, (int)Math.Floor(scaled));
        return Mix(colors[index], colors[index + 1], SmoothStep(scaled - index));
    }

    private static Color Mix(Color start, Color end, double amount)
    {
        double t = Math.Max(0.0, Math.Min(1.0, amount));
        return Color.FromArgb(
            255,
            Clamp((int)Math.Round(start.R + (end.R - start.R) * t)),
            Clamp((int)Math.Round(start.G + (end.G - start.G) * t)),
            Clamp((int)Math.Round(start.B + (end.B - start.B) * t))
        );
    }

    private static double SmoothStep(double value)
    {
        double t = Math.Max(0.0, Math.Min(1.0, value));
        return t * t * (3.0 - 2.0 * t);
    }

    private static Bitmap ComposeLegacy(Bitmap background, Bitmap foreground, bool circular)
    {
        var output = new Bitmap(AdaptiveCanvas, AdaptiveCanvas, PixelFormat.Format32bppArgb);
        using (Graphics graphics = Graphics.FromImage(output))
        using (GraphicsPath clip = circular
            ? CreateEllipsePath(AdaptiveCanvas)
            : CreateRoundedRectanglePath(AdaptiveCanvas, 92))
        {
            ConfigureHighQuality(graphics);
            graphics.SetClip(clip);
            graphics.DrawImage(background, 0, 0, AdaptiveCanvas, AdaptiveCanvas);
            graphics.DrawImage(foreground, 0, 0, AdaptiveCanvas, AdaptiveCanvas);
        }
        return output;
    }

    private static GraphicsPath CreateEllipsePath(int size)
    {
        var path = new GraphicsPath();
        path.AddEllipse(0, 0, size, size);
        path.CloseFigure();
        return path;
    }

    private static GraphicsPath CreateRoundedRectanglePath(int size, int radius)
    {
        var path = new GraphicsPath();
        int diameter = radius * 2;
        path.AddArc(0, 0, diameter, diameter, 180, 90);
        path.AddArc(size - diameter, 0, diameter, diameter, 270, 90);
        path.AddArc(size - diameter, size - diameter, diameter, diameter, 0, 90);
        path.AddArc(0, size - diameter, diameter, diameter, 90, 90);
        path.CloseFigure();
        return path;
    }

    private static void WriteDensity(
        string resourcePath,
        string density,
        int size,
        Bitmap legacy,
        Bitmap round
    )
    {
        string directory = Path.Combine(resourcePath, "mipmap-" + density);
        Directory.CreateDirectory(directory);
        using (Bitmap scaledLegacy = Resize(legacy, size))
        using (Bitmap scaledRound = Resize(round, size))
        {
            // Use a new resource id so OEM launchers cannot keep serving the
            // cached icon from an earlier APK after an in-place upgrade.
            scaledLegacy.Save(Path.Combine(directory, "ic_launcher_v3.png"), ImageFormat.Png);
            scaledRound.Save(Path.Combine(directory, "ic_launcher_round_v3.png"), ImageFormat.Png);
        }
    }

    private static Bitmap Resize(Bitmap source, int size)
    {
        var output = new Bitmap(size, size, PixelFormat.Format32bppArgb);
        using (Graphics graphics = Graphics.FromImage(output))
        {
            ConfigureHighQuality(graphics);
            graphics.CompositingMode = CompositingMode.SourceCopy;
            graphics.DrawImage(source, 0, 0, size, size);
        }
        return output;
    }

    private static void ConfigureHighQuality(Graphics graphics)
    {
        graphics.CompositingQuality = CompositingQuality.HighQuality;
        graphics.InterpolationMode = InterpolationMode.HighQualityBicubic;
        graphics.PixelOffsetMode = PixelOffsetMode.HighQuality;
        graphics.SmoothingMode = SmoothingMode.AntiAlias;
    }

    private static int Clamp(int value)
    {
        return Math.Max(0, Math.Min(255, value));
    }

}
'@

if ($PSVersionTable.PSEdition -eq "Desktop") {
    Add-Type -TypeDefinition $generatorSource -ReferencedAssemblies System.Drawing
} else {
    $drawingAssemblies = ([AppContext]::GetData("TRUSTED_PLATFORM_ASSEMBLIES") -split ';') |
        Where-Object { $_ -and (Test-Path -LiteralPath $_) } |
        Select-Object -Unique
    Add-Type -TypeDefinition $generatorSource -ReferencedAssemblies $drawingAssemblies
}
[FusionPlayAndroidIconGenerator]::Generate($sourcePath, $resourcePath)
