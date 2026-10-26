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
uniform mat3 tex_matrix;
uniform vec4 clip_rect;
uniform float clip_radius;
uniform vec2 clip_scale;
uniform vec3 grain;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
void main() {
    vec2 delta = v_coords - tex_matrix[2].xy;
    float det = tex_matrix[0].x * tex_matrix[1].y - tex_matrix[1].x * tex_matrix[0].y;
    vec2 p = vec2(tex_matrix[1].y * delta.x - tex_matrix[1].x * delta.y,
                  tex_matrix[0].x * delta.y - tex_matrix[0].y * delta.x) / det;
    p /= clip_scale;
    float r = min(clip_radius, min(clip_rect.z * 0.5, clip_rect.w));
    vec2 q = vec2(abs(p.x - clip_rect.x - clip_rect.z * 0.5) - clip_rect.z * 0.5 + r,
                  p.y - clip_rect.y - clip_rect.w + r);
    float distance = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
    distance = max(distance, clip_rect.y - p.y);
    float coverage = 1.0 - smoothstep(-0.5, 0.5, distance * min(clip_scale.x, clip_scale.y));
    vec4 color = texture2D(tex, v_coords);
    if (grain.x > 0.0) {
        vec2 cell = mod(floor(gl_FragCoord.xy / grain.y), 256.0);
        vec3 hash = fract(cell.xyx * 0.1031);
        hash += dot(hash, hash.yzx + 33.33);
        float random = fract((hash.x + hash.y) * hash.z);
        float levels = floor(grain.x * 255.0 + 0.5);
        float noise = (floor(random * levels) - max(levels - 1.0, 0.0) * (0.5 - grain.z)) / 255.0;
        color.rgb = clamp(color.rgb + noise, 0.0, 1.0);
    }
#if defined(NO_ALPHA)
    color.a = 1.0;
#endif
    color *= alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0) color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color * coverage;
}
