package artwork

import (
	"bytes"
	"image/color"
	"image/png"
	"math"
)

// Palette contains bright accents sampled from the cover.
type Palette [3]color.RGBA

var neutralPalette = Palette{{180, 180, 180, 255}, {210, 210, 210, 255}, {240, 240, 240, 255}}

// Colors favours saturated highlights over neutral backgrounds. Sampling is
// bounded, and runs when artwork loads, never on an animation frame.
func Colors(data []byte) Palette {
	fallback := neutralPalette
	img, err := png.Decode(bytes.NewReader(data))
	if err != nil {
		return fallback
	}
	type bucket struct{ r, g, b, weight float64 }
	var bins [4096]bucket
	bounds := img.Bounds()
	for y := bounds.Min.Y; y < bounds.Max.Y; y += max(1, bounds.Dy()/96) {
		for x := bounds.Min.X; x < bounds.Max.X; x += max(1, bounds.Dx()/96) {
			c := color.NRGBAModel.Convert(img.At(x, y)).(color.NRGBA)
			if c.A < 192 {
				continue
			}
			r, g, b := float64(c.R)/255, float64(c.G)/255, float64(c.B)/255
			v, low := max(r, g, b), min(r, g, b)
			if v < 0.18 {
				continue
			}
			saturation := (v - low) / v
			if saturation < 0.22 {
				continue
			}
			weight := saturation * saturation * v * v
			bin := &bins[int(c.R>>4)<<8|int(c.G>>4)<<4|int(c.B>>4)]
			bin.r += r * weight
			bin.g += g * weight
			bin.b += b * weight
			bin.weight += weight
		}
	}
	var result Palette
	for i := range result {
		best, score := -1, 0.0
		for j, bin := range bins {
			if bin.weight == 0 {
				continue
			}
			r, g, b := bin.r/bin.weight, bin.g/bin.weight, bin.b/bin.weight
			distance := 1.0
			for k := 0; k < i; k++ {
				c := result[k]
				// Compare hue independently of the brightness lift below.
				v := max(r, g, b)
				cv := float64(max(c.R, c.G, c.B))
				d := math.Pow(r/v-float64(c.R)/cv, 2) + math.Pow(g/v-float64(c.G)/cv, 2) + math.Pow(b/v-float64(c.B)/cv, 2)
				distance = min(distance, d)
			}
			candidate := math.Sqrt(bin.weight) * distance
			if candidate > score {
				best, score = j, candidate
			}
		}
		if best < 0 {
			if i == 0 {
				return fallback
			}
			result[i] = result[i-1]
			continue
		}
		bin := bins[best]
		r, g, b := bin.r/bin.weight, bin.g/bin.weight, bin.b/bin.weight
		scale := max(1, 0.88/max(r, g, b))
		result[i] = color.RGBA{uint8(math.Round(r * scale * 255)), uint8(math.Round(g * scale * 255)), uint8(math.Round(b * scale * 255)), 255}
	}
	return result
}

// At interpolates between the extracted accents along a bar's height.
func (p Palette) At(t float64) color.RGBA {
	if p[0].A == 0 {
		p = neutralPalette
	}
	t = min(1, max(0, t)) * 2
	i := min(1, int(t))
	f := t - float64(i)
	a, b := p[i], p[i+1]
	mix := func(x, y uint8) uint8 { return uint8(math.Round(float64(x)*(1-f) + float64(y)*f)) }
	return color.RGBA{mix(a.R, b.R), mix(a.G, b.G), mix(a.B, b.B), 255}
}
