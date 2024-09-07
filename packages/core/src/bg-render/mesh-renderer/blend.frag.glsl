precision highp float;

varying vec2 v_uv;
uniform sampler2D u_texture;
uniform float u_alpha;

/* Gradient noise from Jorge Jimenez's presentation: */
/* http://www.iryoku.com/next-generation-post-processing-in-call-of-duty-advanced-warfare */
float gradientNoise(in vec2 uv) {
    return fract(52.9829189 * fract(dot(uv, vec2(0.06711056, 0.00583715))));
}

vec2 rot(vec2 v, float a) {
    float s = sin(a);
    float c = cos(a);
    mat2 m = mat2(c, s, -s, c);
    return m * v;
}

void main() {
    float dither = (1.0 / 255.0) * gradientNoise(gl_FragCoord.xy) - (0.5 / 255.0);

    vec4 result = texture2D(u_texture, v_uv);
    gl_FragColor = result * vec4(1.0, 1.0, 1.0, u_alpha) + vec4(dither);
}
