## [INSTRUCTION] Skill Rules

{% for skill in mentioned -%}
Skill: {{ skill.name }}
{{ skill.content }}
---
{% endfor -%}
