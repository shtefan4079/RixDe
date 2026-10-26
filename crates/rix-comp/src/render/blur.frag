#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
uniform vec2 step_size;
uniform vec2 half_texel;
uniform float radius;
uniform float pass_mode;
uniform vec2 grain_origin;
uniform vec3 grain;
uniform vec3 correction;
varying vec2 v_coords;
vec4 sample_at(vec2 uv) {
    return texture2D(tex, clamp(uv, half_texel, vec2(1.0) - half_texel));
}
void main() {
    vec4 color = sample_at(v_coords);
    if (pass_mode > 2.5) {
        vec2 d = half_texel * radius;
        color = (sample_at(v_coords + vec2(-2.0*d.x, 0.0))
               + sample_at(v_coords + vec2( 2.0*d.x, 0.0))
               + sample_at(v_coords + vec2(0.0, -2.0*d.y))
               + sample_at(v_coords + vec2(0.0,  2.0*d.y))
               + 2.0 * (sample_at(v_coords + d) + sample_at(v_coords - d)
               + sample_at(v_coords + vec2(d.x, -d.y))
               + sample_at(v_coords + vec2(-d.x, d.y)))) / 12.0;
    } else if (pass_mode > 1.5) {
        vec2 d = half_texel * radius;
        color = (4.0 * color + sample_at(v_coords + d) + sample_at(v_coords - d)
               + sample_at(v_coords + vec2(d.x, -d.y))
               + sample_at(v_coords + vec2(-d.x, d.y))) / 8.0;
    } else if (pass_mode > 0.5) {
        float weights = 1.0;
        float sigma = max(radius / 3.0, 0.5);
        for (int i = 1; i <= 128; ++i) {
            if (float(i) > radius) break;
            float distance = float(i);
            float weight = exp(-0.5 * distance * distance / (sigma * sigma));
            color += (sample_at(v_coords + step_size * distance)
                    + sample_at(v_coords - step_size * distance)) * weight;
            weights += 2.0 * weight;
        }
        color /= weights;
    } else {
        vec3 rgb = (color.rgb - 0.5) * correction.y + 0.5;
        float luminance = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
        rgb = mix(vec3(luminance), rgb, correction.z) * correction.x;
        vec2 cell = mod(floor((gl_FragCoord.xy + grain_origin) / grain.y), 256.0);
        vec3 hash = fract(cell.xyx * 0.1031);
        hash += dot(hash, hash.yzx + 33.33);
        float random = fract((hash.x + hash.y) * hash.z);
        float levels = floor(grain.x * 255.0 + 0.5);
        float noise = (floor(random * levels) - max(levels - 1.0, 0.0) * (0.5 - grain.z)) / 255.0;
        color = vec4(clamp(rgb + noise, 0.0, 1.0), 1.0);
    }
    gl_FragColor = color * alpha;
}
