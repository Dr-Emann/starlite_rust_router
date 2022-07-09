import typing

BaseRoute = typing.Any
Scope = typing.MutableMapping[str, typing.Any]
Message = typing.MutableMapping[str, typing.Any]

Receive = typing.Callable[[], typing.Awaitable[Message]]
Send = typing.Callable[[Message], typing.Awaitable[None]]

ASGIApp = typing.Callable[[Scope, Receive, Send], typing.Awaitable[None]]



class RouteMap:
    def __init__(self, app: typing.Any): ...

    def add_routes(self, routes: typing.Collection[BaseRoute]): ...

    def resolve_route(self, scope: Scope) -> ASGIApp: ...
